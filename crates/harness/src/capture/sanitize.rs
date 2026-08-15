use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};

use super::allowlist::{allows, named_kind};
use super::filesystem::has_windows_reparse_point;
use super::types::{Channel, PartialRawCapture, Provider, RawCapture};

#[derive(Clone, Debug)]
pub struct SanitizationReport {
    pub events_path: PathBuf,
    pub manifest_path: PathBuf,
    pub events_bytes: Vec<u8>,
    pub manifest_bytes: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum SanitizationError {
    #[error("raw capture could not be read")]
    ReadRaw {
        #[source]
        source: std::io::Error,
    },
    #[error("raw capture is not valid capture JSON")]
    InvalidRaw {
        #[source]
        source: serde_json::Error,
    },
    #[error("incomplete capture evidence cannot be sanitized or promoted")]
    IncompleteCapture,
    #[error("staging output must be below .comet-provider-captures/staging")]
    UnsafeOutputDirectory,
    #[error("capture contains an unrecognized absolute path at {location}")]
    UnrecognizedAbsolutePath { location: String },
    #[error("capture contains a secret-like field at {location}")]
    SecretLikeField { location: String },
    #[error("capture contains a secret-like value at {location}")]
    SecretLikeValue { location: String },
    #[error("capture contains a sensitive object key at {location}")]
    SensitiveObjectKey { location: String },
    #[error("capture channel contains unparseable structured JSON at sequence {sequence}")]
    UnparseableStructuredPayload { sequence: u64 },
    #[error("Claude capture command has invalid resume arguments at {location}")]
    InvalidClaudeResumeCommand { location: String },
    #[error("sanitized capture could not be written")]
    WriteOutput {
        #[source]
        source: std::io::Error,
    },
    #[error("sanitized staging destination is busy; retry after the other publisher finishes")]
    PublicationBusy {
        #[source]
        source: std::io::Error,
    },
    #[error("sanitized staging destination already contains different evidence")]
    PublicationConflict {
        #[source]
        source: std::io::Error,
    },
    #[error("sanitized capture could not be encoded")]
    EncodeOutput {
        #[source]
        source: serde_json::Error,
    },
}

/// Per-capture redaction state: which values have already been assigned a
/// placeholder, and the counters that feed the manifest.
///
/// Numbering is keyed on the original value, not on a counter, so equal
/// values collide into the same placeholder deliberately — a join that held
/// on the wire still holds in the archive. `named` groups are the six
/// identifier kinds from `allowlist::named_kind` (plus the internal `PROSE`
/// group for non-JSON stderr payloads); `generic` is the numbered fallback
/// (`<V1>`, `<V2>`, …) for every other unlisted scalar. Both are `Vec`, not
/// `HashMap`, because encounter order is what makes a run byte-deterministic
/// — the publication tests assert on that determinism directly.
#[derive(Default)]
struct Redactor {
    paths: Vec<PathRedaction>,
    named: BTreeMap<&'static str, Vec<(Value, String)>>,
    generic: Vec<(Value, String)>,
    counts: BTreeMap<String, u64>,
}

#[derive(Clone)]
struct PathRedaction {
    values: Vec<String>,
    placeholder: &'static str,
    kind: &'static str,
}

/// Convert one raw capture into reviewable staging artifacts.
///
/// All transformation and validation happens in memory. A rejected capture never creates its
/// output directory, and errors identify only the value's structural location.
pub fn sanitize_dir(
    raw_dir: &Path,
    output_dir: &Path,
) -> Result<SanitizationReport, SanitizationError> {
    if !is_staging_output(output_dir) {
        return Err(SanitizationError::UnsafeOutputDirectory);
    }

    let capture_path = raw_dir.join("capture.json");
    let partial_path = raw_dir.join("partial-capture.json");
    if partial_path.exists() {
        let bytes =
            std::fs::read(partial_path).map_err(|source| SanitizationError::ReadRaw { source })?;
        let _: PartialRawCapture = serde_json::from_slice(&bytes)
            .map_err(|source| SanitizationError::InvalidRaw { source })?;
        return Err(SanitizationError::IncompleteCapture);
    }
    let bytes =
        std::fs::read(capture_path).map_err(|source| SanitizationError::ReadRaw { source })?;
    let capture: RawCapture = serde_json::from_slice(&bytes)
        .map_err(|source| SanitizationError::InvalidRaw { source })?;
    let mut redactor = Redactor::new(&capture);

    let mut payloads = Vec::with_capacity(capture.events.len());
    for event in &capture.events {
        match serde_json::from_str::<Value>(&event.payload) {
            Ok(payload) => payloads.push(Payload::Json(payload)),
            Err(_) if event.channel == Channel::Stderr => {
                payloads.push(Payload::Text(event.payload.clone()))
            }
            Err(_) => {
                return Err(SanitizationError::UnparseableStructuredPayload {
                    sequence: event.sequence,
                });
            }
        }
    }

    // Events are sanitized before `command`, deliberately: the Claude resume
    // argv below needs the session id's placeholder already assigned, and a
    // session id normally first appears inside an event payload, not in
    // `command` itself.
    let mut events_bytes = Vec::new();
    for (event, payload) in capture.events.iter().zip(&mut payloads) {
        let payload = match payload {
            Payload::Json(value) => {
                redactor.sanitize_value_tree(value, capture.provider, "", "", "event.payload")?;
                serde_json::to_string(value)
                    .map_err(|source| SanitizationError::EncodeOutput { source })?
            }
            Payload::Text(text) => {
                let placeholder = redactor.placeholder_for_prose(text);
                *text = placeholder.clone();
                placeholder
            }
        };
        let line = SanitizedEvent {
            sequence: event.sequence,
            channel: event.channel,
            payload,
        };
        serde_json::to_writer(&mut events_bytes, &line)
            .map_err(|source| SanitizationError::EncodeOutput { source })?;
        events_bytes.push(b'\n');
    }

    let mut command = serde_json::to_value(&capture.command)
        .map_err(|source| SanitizationError::EncodeOutput { source })?;
    if capture.provider == Provider::Claude {
        redactor.sanitize_claude_resume_argv(&mut command, &capture.scenario)?;
    }
    redactor.sanitize_nonsemantic_value(&mut command, "command")?;

    let mut cli_version = capture.cli_version.clone();
    redactor.sanitize_paths_and_validate(&mut cli_version, "cli_version")?;
    let mut normalized_cli_version = capture.cli_version.trim().to_owned();
    redactor.sanitize_paths_and_validate(&mut normalized_cli_version, "normalized_cli_version")?;
    let mut scenario = capture.scenario.clone();
    redactor.sanitize_paths_and_validate(&mut scenario, "scenario")?;
    let mut purpose = capture.purpose.clone();
    redactor.sanitize_paths_and_validate(&mut purpose, "purpose")?;
    let mut platform = serde_json::to_value(&capture.platform)
        .map_err(|source| SanitizationError::EncodeOutput { source })?;
    redactor.sanitize_nonsemantic_value(&mut platform, "platform")?;
    let channels: Vec<Channel> = capture.events.iter().fold(Vec::new(), |mut seen, event| {
        if !seen.contains(&event.channel) {
            seen.push(event.channel);
        }
        seen
    });
    let manifest = json!({
        "schema_version": 1,
        "source": "capture.json",
        "provider": capture.provider,
        "cli_version": cli_version,
        "normalized_cli_version": normalized_cli_version,
        "captured_at_unix_ms": capture.captured_at_unix_ms,
        "scenario": scenario,
        "purpose": purpose,
        "platform": platform,
        "command": command,
        "channels": channels,
        "exit_code": capture.exit_code,
        "placeholders": redactor.placeholder_definitions(),
        "redaction_counts": redactor.counts,
    });
    let mut manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|source| SanitizationError::EncodeOutput { source })?;
    manifest_bytes.push(b'\n');

    let events_path = output_dir.join("events.jsonl");
    let manifest_path = output_dir.join("manifest.json");
    publish_staging_pair_with(output_dir, &events_bytes, &manifest_bytes, |_| Ok(())).map_err(
        |source| match source.kind() {
            std::io::ErrorKind::WouldBlock => SanitizationError::PublicationBusy { source },
            std::io::ErrorKind::AlreadyExists => SanitizationError::PublicationConflict { source },
            _ => SanitizationError::WriteOutput { source },
        },
    )?;

    Ok(SanitizationReport {
        events_path,
        manifest_path,
        events_bytes,
        manifest_bytes,
    })
}

fn publish_staging_pair_with<F>(
    output_dir: &Path,
    events: &[u8],
    manifest: &[u8],
    after_events: F,
) -> std::io::Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
{
    publish_staging_pair_with_commit(output_dir, events, manifest, after_events, |from, to| {
        std::fs::rename(from, to)
    })
}

fn publish_staging_pair_with_commit<F, C>(
    output_dir: &Path,
    events: &[u8],
    manifest: &[u8],
    after_events: F,
    commit: C,
) -> std::io::Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
    C: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    let parent = output_dir.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "staging destination has no parent",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let output_name = output_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("capture");
    let publication_lock = PublicationLock::acquire(parent, output_name)?;
    let result = (|| {
        let temporary_prefix = format!(".{output_name}.sanitize-");
        let temporary = parent.join(format!("{temporary_prefix}{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&temporary)?;

        let prepared = (|| {
            write_synced(&temporary.join("events.jsonl"), events)?;
            after_events(&temporary)?;
            write_synced(&temporary.join("manifest.json"), manifest)?;
            Ok(())
        })();
        if let Err(error) = prepared {
            let _ = remove_verified_generated_dir(&temporary, parent, &temporary_prefix);
            return Err(error);
        }

        if output_dir.exists() {
            let identical = match destination_is_exact_pair(output_dir, events, manifest) {
                Ok(identical) => identical,
                Err(error) => {
                    let _ = remove_verified_generated_dir(&temporary, parent, &temporary_prefix);
                    return Err(error);
                }
            };
            remove_verified_generated_dir(&temporary, parent, &temporary_prefix)?;
            if identical {
                return Ok(());
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "sanitized staging destination already contains different evidence",
            ));
        }

        if let Err(error) = commit(&temporary, output_dir) {
            let _ = remove_verified_generated_dir(&temporary, parent, &temporary_prefix);
            return Err(error);
        }
        Ok(())
    })();
    match publication_lock.release() {
        Ok(()) => result,
        Err(error) => Err(error),
    }
}

fn destination_is_exact_pair(
    output_dir: &Path,
    expected_events: &[u8],
    expected_manifest: &[u8],
) -> std::io::Result<bool> {
    let directory_metadata = std::fs::symlink_metadata(output_dir)?;
    if !directory_metadata.file_type().is_dir()
        || directory_metadata.file_type().is_symlink()
        || has_windows_reparse_point(&directory_metadata)
    {
        return Ok(false);
    }

    let mut events_match = false;
    let mut manifest_match = false;
    let mut entry_count = 0;
    for entry in std::fs::read_dir(output_dir)? {
        let entry = entry?;
        entry_count += 1;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || has_windows_reparse_point(&metadata)
        {
            return Ok(false);
        }
        if entry.file_name() == "events.jsonl" {
            events_match = std::fs::read(entry.path())? == expected_events;
        } else if entry.file_name() == "manifest.json" {
            manifest_match = std::fs::read(entry.path())? == expected_manifest;
        } else {
            return Ok(false);
        }
    }
    Ok(entry_count == 2 && events_match && manifest_match)
}

struct PublicationLock {
    _file: std::fs::File,
    path: PathBuf,
    parent: PathBuf,
    name: String,
    owner: String,
}

impl PublicationLock {
    fn acquire(parent: &Path, output_name: &str) -> std::io::Result<Self> {
        let parent = std::fs::canonicalize(parent)?;
        let name = format!(".{output_name}.publish.lock");
        let path = parent.join(&name);
        if path.parent() != Some(parent.as_path()) || path.file_name() != Some(name.as_ref()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing to create an unverified publication lock",
            ));
        }
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "sanitized staging destination is busy",
                ));
            }
            Err(error) => return Err(error),
        };
        let owner = uuid::Uuid::new_v4().to_string();
        {
            use std::io::Write as _;
            file.write_all(owner.as_bytes())?;
            file.sync_all()?;
        }
        Ok(Self {
            _file: file,
            path,
            parent,
            name,
            owner,
        })
    }

    fn release(self) -> std::io::Result<()> {
        remove_verified_generated_file(&self.path, &self.parent, &self.name, self.owner.as_bytes())
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn remove_verified_generated_dir(
    generated: &Path,
    parent: &Path,
    expected_prefix: &str,
) -> std::io::Result<()> {
    let resolved_parent = std::fs::canonicalize(parent)?;
    let resolved_generated = std::fs::canonicalize(generated)?;
    let valid_name = resolved_generated
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(expected_prefix));
    if resolved_generated.parent() != Some(resolved_parent.as_path()) || !valid_name {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to clean an unverified staging sibling",
        ));
    }
    std::fs::remove_dir_all(resolved_generated)
}

fn remove_verified_generated_file(
    generated: &Path,
    parent: &Path,
    expected_name: &str,
    expected_contents: &[u8],
) -> std::io::Result<()> {
    let resolved_parent = std::fs::canonicalize(parent)?;
    let resolved_generated = std::fs::canonicalize(generated)?;
    let metadata = std::fs::symlink_metadata(generated)?;
    let valid_name = resolved_generated.file_name() == Some(expected_name.as_ref());
    if resolved_generated.parent() != Some(resolved_parent.as_path())
        || !valid_name
        || !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || has_windows_reparse_point(&metadata)
        || std::fs::read(generated)? != expected_contents
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to clean an unverified publication lock",
        ));
    }
    std::fs::remove_file(resolved_generated)
}

#[derive(Serialize)]
struct SanitizedEvent {
    sequence: u64,
    channel: Channel,
    payload: String,
}

enum Payload {
    Json(Value),
    Text(String),
}

impl Redactor {
    fn new(capture: &RawCapture) -> Self {
        let mut redactor = Self::default();
        redactor.add_path(capture.redaction_roots.cwd.as_deref(), "<CWD>", "cwd_path");
        redactor.add_path(
            capture.redaction_roots.repo.as_deref(),
            "<REPO>",
            "repo_path",
        );
        redactor.add_path(
            capture.redaction_roots.home.as_deref(),
            "<HOME>",
            "home_path",
        );
        redactor.add_path(
            capture.redaction_roots.temp.as_deref(),
            "<TEMP>",
            "temp_path",
        );
        redactor.add_path(
            capture.redaction_roots.codex_home.as_deref(),
            "<CODEX_HOME>",
            "codex_home_path",
        );
        redactor.add_path(
            capture.redaction_roots.approval_target.as_deref(),
            "<APPROVAL_TARGET>",
            "approval_target_path",
        );
        redactor.add_path(
            capture.redaction_roots.trusted_powershell.as_deref(),
            "<TRUSTED_POWERSHELL>",
            "trusted_powershell_path",
        );
        redactor
    }

    fn add_path(&mut self, value: Option<&str>, placeholder: &'static str, kind: &'static str) {
        let Some(value) = value.filter(|value| !value.is_empty()) else {
            return;
        };
        if self
            .paths
            .iter()
            .any(|redaction| redaction.values.iter().any(|known| known == value))
        {
            return;
        }
        let mut values = vec![value.to_owned()];
        let slash = value.replace('\\', "/");
        let backslash = value.replace('/', "\\");
        for variant in [slash, backslash] {
            if !values.contains(&variant) {
                values.push(variant);
            }
        }
        for variant in values.clone() {
            if is_drive_absolute(&variant) {
                values.push(format!(r"\\?\{variant}"));
                values.push(format!("//?/{variant}"));
            }
        }
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        self.paths.push(PathRedaction {
            values,
            placeholder,
            kind,
        });
        self.paths
            .sort_by_key(|path| std::cmp::Reverse(path.values.first().map_or(0, String::len)));
    }

    /// The allowlist walker. For each scalar (`String`/`Number`) at dotted
    /// path `path`: if its field name looks credential-shaped, reject the
    /// whole capture regardless of allow status — `is_secret_field` is a
    /// tripwire independent of the allowlist, because a name like
    /// `apiKey`/`authorization` showing up anywhere is itself the alarm.
    /// Otherwise, if `path` is on the provider's allowlist, keep the value
    /// verbatim and run the fail-closed scan over it — an allowlisted path
    /// is a decision about the *field*, not a licence for whatever a
    /// provider happens to put in it. If it is not allowed, replace it with
    /// a placeholder. `Bool`/`Null` are never redacted: they carry no
    /// free-form data.
    ///
    /// `Array`/`Object` recurse; an array element's path grows `[]` (never
    /// an index, so every element of an array shares one allowlist
    /// decision) and its `key` stays the containing field's key, so
    /// `is_secret_field`/`named_kind` still see a meaningful field name for
    /// each scalar inside e.g. `"tags": ["sk-ant-…"]`.
    fn sanitize_value_tree(
        &mut self,
        value: &mut Value,
        provider: Provider,
        path: &str,
        key: &str,
        location: &str,
    ) -> Result<(), SanitizationError> {
        match value {
            Value::Array(items) => {
                let child_path = format!("{path}[]");
                for (index, item) in items.iter_mut().enumerate() {
                    self.sanitize_value_tree(
                        item,
                        provider,
                        &child_path,
                        key,
                        &format!("{location}[{index}]"),
                    )?;
                }
                Ok(())
            }
            Value::Object(object) => {
                let keys: Vec<String> = object.keys().cloned().collect();
                for (index, child_key) in keys.into_iter().enumerate() {
                    let child_location = format!("{location}.object[{index}]");
                    self.validate_key(&child_key, &child_location)?;
                    let child_path = format!("{path}.{child_key}");
                    let child = object.get_mut(&child_key).expect("key came from object");
                    self.sanitize_value_tree(
                        child,
                        provider,
                        &child_path,
                        &child_key,
                        &child_location,
                    )?;
                }
                Ok(())
            }
            Value::String(_) | Value::Number(_) => {
                self.sanitize_scalar(value, provider, path, key, location)
            }
            _ => Ok(()),
        }
    }

    fn sanitize_scalar(
        &mut self,
        value: &mut Value,
        provider: Provider,
        path: &str,
        key: &str,
        location: &str,
    ) -> Result<(), SanitizationError> {
        if is_secret_field(key, value) {
            return Err(SanitizationError::SecretLikeField {
                location: location.to_owned(),
            });
        }
        if allows(provider, path) && !is_mcp_tool_identity(value) {
            if let Value::String(text) = value {
                self.sanitize_paths_and_validate(text, location)?;
            }
            return Ok(());
        }
        let placeholder = self.placeholder_for(key, value);
        *value = Value::String(placeholder);
        Ok(())
    }

    /// The placeholder for `value` under `key`'s named group —
    /// `named_kind(key)` if it is one of the six identifier leaves
    /// (`<SESSION_1>`, …), otherwise the generic `<V1>` fallback.
    fn placeholder_for(&mut self, key: &str, value: &Value) -> String {
        self.resolve(value, named_kind(key))
    }

    /// The single stderr-prose placeholder group: non-JSON payloads (always
    /// `Channel::Stderr`; anything else fails the capture earlier) collapse
    /// to `<PROSE_n>`, with equal text sharing a number like every other
    /// group.
    fn placeholder_for_prose(&mut self, text: &str) -> String {
        self.resolve(&Value::String(text.to_owned()), Some("PROSE"))
    }

    /// Identity comes from the value, not from a kind: `resolve` first
    /// searches *every* group — named and generic — for `value`, regardless
    /// of `fallback_group`, so a value already redacted under one field's
    /// name (say `sessionId`) reuses that exact placeholder when the same
    /// literal value shows up again under an unrelated field (say `echo`).
    /// Only a genuinely new value gets a fresh placeholder, assigned into
    /// `fallback_group` (or the generic `<V n>` fallback when `None`) using
    /// that group's own next number.
    ///
    /// Encounter order, not a `HashMap`, is what makes this deterministic:
    /// both `named` and `generic` are `Vec`s scanned linearly, so the same
    /// capture always numbers the same way — the property the publication
    /// tests assert on as byte determinism.
    fn resolve(&mut self, value: &Value, fallback_group: Option<&'static str>) -> String {
        for (&name, group) in &self.named {
            if let Some((_, placeholder)) = group.iter().find(|(known, _)| known == value) {
                let placeholder = placeholder.clone();
                *self.counts.entry(name.to_ascii_lowercase()).or_default() += 1;
                return placeholder;
            }
        }
        if let Some((_, placeholder)) = self.generic.iter().find(|(known, _)| known == value) {
            let placeholder = placeholder.clone();
            *self.counts.entry("v".to_owned()).or_default() += 1;
            return placeholder;
        }
        match fallback_group {
            Some(name) => {
                let group = self.named.entry(name).or_default();
                let placeholder = format!("<{name}_{}>", group.len() + 1);
                group.push((value.clone(), placeholder.clone()));
                *self.counts.entry(name.to_ascii_lowercase()).or_default() += 1;
                placeholder
            }
            None => {
                let placeholder = format!("<V{}>", self.generic.len() + 1);
                self.generic.push((value.clone(), placeholder.clone()));
                *self.counts.entry("v".to_owned()).or_default() += 1;
                placeholder
            }
        }
    }

    fn sanitize_nonsemantic_value(
        &mut self,
        value: &mut Value,
        location: &str,
    ) -> Result<(), SanitizationError> {
        match value {
            Value::Array(values) => {
                for (index, value) in values.iter_mut().enumerate() {
                    self.sanitize_nonsemantic_value(value, &format!("{location}[{index}]"))?;
                }
            }
            Value::Object(object) => {
                for (index, (key, value)) in object.iter_mut().enumerate() {
                    let child_location = format!("{location}.object[{index}]");
                    self.validate_key(key, &child_location)?;
                    if is_secret_field(key, value) {
                        return Err(SanitizationError::SecretLikeField {
                            location: child_location,
                        });
                    }
                    self.sanitize_nonsemantic_value(value, &child_location)?;
                }
            }
            Value::String(text) => self.sanitize_paths_and_validate(text, location)?,
            _ => {}
        }
        Ok(())
    }

    fn validate_key(&mut self, key: &str, location: &str) -> Result<(), SanitizationError> {
        let mut sanitized = key.to_owned();
        self.sanitize_paths_and_validate(&mut sanitized, location)?;
        if sanitized != key {
            return Err(SanitizationError::SensitiveObjectKey {
                location: location.to_owned(),
            });
        }
        Ok(())
    }

    /// Scenarios whose argv may legitimately carry `--resume=<id>`.
    ///
    /// The rule this gates is fail-closed on purpose: a `--resume` in a
    /// capture that had no business resuming is evidence the command was not
    /// what the scenario says, and that must stop promotion rather than be
    /// sanitized away. What is wrong with it is only that the permitted set is
    /// a literal here, so every new resume-bearing scenario is rejected until
    /// someone finds this function — `checklist-resume` was, and the error
    /// (`Claude capture command has invalid resume arguments at command.args`)
    /// names the argv rather than the missing registration. **D60** is the
    /// structural fix: one scenario table the help text, `supported_pair` and
    /// this list all read from.
    const RESUMING_SCENARIOS: &'static [&'static str] = &["resume", "checklist-resume"];

    fn sanitize_claude_resume_argv(
        &mut self,
        command: &mut Value,
        scenario: &str,
    ) -> Result<(), SanitizationError> {
        let Some(args) = command.get_mut("args").and_then(Value::as_array_mut) else {
            return Err(SanitizationError::InvalidClaudeResumeCommand {
                location: "command.args".to_owned(),
            });
        };
        let resume_like: Vec<usize> = args
            .iter()
            .enumerate()
            .filter_map(|(index, arg)| {
                arg.as_str()
                    .is_some_and(|arg| arg.starts_with("--resume"))
                    .then_some(index)
            })
            .collect();
        if !Self::RESUMING_SCENARIOS.contains(&scenario) {
            if resume_like.is_empty() {
                return Ok(());
            }
            return Err(SanitizationError::InvalidClaudeResumeCommand {
                location: "command.args".to_owned(),
            });
        }

        let Some(&index) = resume_like
            .as_slice()
            .first()
            .filter(|_| resume_like.len() == 1)
        else {
            return Err(SanitizationError::InvalidClaudeResumeCommand {
                location: "command.args".to_owned(),
            });
        };
        let Some(raw_session_id) = args[index]
            .as_str()
            .and_then(|arg| arg.strip_prefix("--resume="))
            .filter(|session_id| !session_id.is_empty())
        else {
            return Err(SanitizationError::InvalidClaudeResumeCommand {
                location: "command.args".to_owned(),
            });
        };
        let Some([(original, placeholder)]) = self.named.get("SESSION").map(Vec::as_slice) else {
            return Err(SanitizationError::InvalidClaudeResumeCommand {
                location: "command.args".to_owned(),
            });
        };
        let Some(original) = original.as_str().filter(|value| !value.is_empty()) else {
            return Err(SanitizationError::InvalidClaudeResumeCommand {
                location: "command.args".to_owned(),
            });
        };
        if raw_session_id != original {
            return Err(SanitizationError::InvalidClaudeResumeCommand {
                location: "command.args".to_owned(),
            });
        }
        args[index] = Value::String(format!("--resume={placeholder}"));
        *self.counts.entry("session".to_owned()).or_default() += 1;
        Ok(())
    }

    fn sanitize_paths_and_validate(
        &mut self,
        text: &mut String,
        location: &str,
    ) -> Result<(), SanitizationError> {
        for path in self.paths.clone() {
            let mut occurrences = 0;
            for value in path.values {
                if path_occurrence_escapes_root(text, &value) {
                    return Err(SanitizationError::UnrecognizedAbsolutePath {
                        location: location.to_owned(),
                    });
                }
                let found = replace_path_occurrences(text, &value, path.placeholder);
                if found != 0 {
                    occurrences += found;
                }
            }
            if occurrences != 0 {
                *self.counts.entry(path.kind.to_owned()).or_default() += occurrences;
            }
        }
        if contains_absolute_path(text) {
            return Err(SanitizationError::UnrecognizedAbsolutePath {
                location: location.to_owned(),
            });
        }
        if contains_secret_value(text) {
            return Err(SanitizationError::SecretLikeValue {
                location: location.to_owned(),
            });
        }
        Ok(())
    }

    fn placeholder_definitions(&self) -> Vec<Value> {
        let mut definitions = Vec::new();
        for (name, values) in &self.named {
            let kind = name.to_ascii_lowercase();
            for (_, placeholder) in values {
                definitions.push(json!({
                    "placeholder": placeholder,
                    "kind": kind,
                }));
            }
        }
        for (_, placeholder) in &self.generic {
            definitions.push(json!({
                "placeholder": placeholder,
                "kind": "v",
            }));
        }
        for path in &self.paths {
            if self.counts.contains_key(path.kind) {
                definitions.push(json!({
                    "placeholder": path.placeholder,
                    "kind": path.kind,
                }));
            }
        }
        definitions
    }
}

/// `pub(crate)`, not private: `allowlist::named_kind` routes leaf lookup through
/// this same rule (see its doc comment), so Claude's `session_id` and Codex's
/// `sessionId` — if it ever used that spelling — collapse to one entry instead
/// of needing a copy of this function that can drift from this one.
pub(crate) fn normalize_field(field: &str) -> String {
    field
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_secret_field(field: &str, value: &Value) -> bool {
    let normalized = normalize_field(field);
    if is_token_counter_field(&normalized) {
        return !value.is_number();
    }
    normalized == "token"
        || [
            "apitoken",
            "accesstoken",
            "authtoken",
            "bearertoken",
            "idtoken",
            "oauthtoken",
            "personalaccesstoken",
            "refreshtoken",
            "sessiontoken",
            "apikey",
            "clientsecret",
            "privatekey",
            "secretkey",
            "signingkey",
            "credential",
            "credentials",
            "password",
            "authorization",
        ]
        .iter()
        .any(|family| normalized.ends_with(family))
        || normalized == "secret"
}

/// A decision this task owns, not Task 1's path review: the tool-name-at-
/// invocation family (`.message.content[].name`, `.event.content_block.name`,
/// `.request.tool_name`, `.last_tool_name`, `.request.display_name`,
/// `.message.content[].content[].tool_name`, `.tool_use_result.matches[]`) is
/// allowlisted, and every sampled value today is a built-in tool name
/// (`Read`, `Bash`, `TaskCreate`). An MCP invocation puts
/// `mcp__<server>__<tool>` in that same field, embedding the server identity
/// `.mcp_servers[].name` and `.tools[]` were both excluded to protect —
/// closing those paths and leaving this one open would undo the fix. A path
/// decision can't express "this field, except when the value looks like
/// this", so the exception lives at the value, checked after the path is
/// already known to be allowed.
fn is_mcp_tool_identity(value: &Value) -> bool {
    value.as_str().is_some_and(|text| text.starts_with("mcp__"))
}

fn is_token_counter_field(field: &str) -> bool {
    matches!(
        field,
        "inputtoken"
            | "inputtokens"
            | "outputtoken"
            | "outputtokens"
            | "cachedinputtokens"
            | "cachecreationinputtokens"
            | "cachereadinputtokens"
            | "reasoningoutputtokens"
            | "maxtoken"
            | "maxtokens"
            | "totaltoken"
            | "totaltokens"
            | "tokencount"
            | "totaltokencount"
            | "tokenusage"
    )
}

fn contains_secret_value(value: &str) -> bool {
    let uppercase = value.to_ascii_uppercase();
    if uppercase.contains("-----BEGIN ") && uppercase.contains(" PRIVATE KEY-----") {
        return true;
    }
    ["sk-ant-", "sk-proj-", "xoxb-", "ghp_", "github_pat_"]
        .iter()
        .any(|prefix| value.contains(prefix))
        || value
            .match_indices("sk-")
            .any(|(index, _)| index == 0 || !value.as_bytes()[index - 1].is_ascii_alphanumeric())
}

fn replace_path_occurrences(text: &mut String, path: &str, placeholder: &str) -> u64 {
    let mut count = 0;
    let mut cursor = 0;
    while let Some(relative) = text[cursor..].find(path) {
        let start = cursor + relative;
        let end = start + path.len();
        let ends_at_path_boundary = end == text.len()
            || text[end..]
                .chars()
                .next()
                .is_some_and(|character| matches!(character, '/' | '\\' | '"' | '\''));
        if ends_at_path_boundary {
            text.replace_range(start..end, placeholder);
            cursor = start + placeholder.len();
            count += 1;
        } else {
            cursor = end;
        }
    }
    count
}

fn path_occurrence_escapes_root(text: &str, root: &str) -> bool {
    let mut cursor = 0;
    while let Some(relative) = text[cursor..].find(root) {
        let end = cursor + relative + root.len();
        let tail = &text[end..];
        if tail.starts_with(['/', '\\']) {
            let path_tail = tail
                .split(|character: char| {
                    character.is_whitespace() || matches!(character, '"' | '\'' | '<' | '>' | '|')
                })
                .next()
                .unwrap_or(tail);
            let mut depth = 0usize;
            for component in path_tail.split(['/', '\\']).filter(|part| !part.is_empty()) {
                match component {
                    "." => {}
                    ".." if depth == 0 => return true,
                    ".." => depth -= 1,
                    _ => depth += 1,
                }
            }
        }
        cursor = end;
    }
    false
}

fn contains_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    for index in 0..bytes.len() {
        if index + 2 < bytes.len()
            && bytes[index].is_ascii_alphabetic()
            && bytes[index + 1] == b':'
            && matches!(bytes[index + 2], b'/' | b'\\')
            && (index == 0 || !bytes[index - 1].is_ascii_alphanumeric())
        {
            return true;
        }
        if index + 1 < bytes.len() && bytes[index] == b'\\' && bytes[index + 1] == b'\\' {
            return true;
        }
        if index + 1 < bytes.len()
            && bytes[index] == b'/'
            && bytes[index + 1] == b'/'
            && (index == 0 || !matches!(bytes[index - 1], b':' | b'/'))
        {
            return true;
        }
        if bytes[index] == b'/'
            && bytes.get(index + 1).is_some_and(|next| {
                !next.is_ascii_whitespace() && !matches!(*next, b'/' | b'>' | b'}' | b']')
            })
            && (index == 0
                || (!bytes[index - 1].is_ascii_alphanumeric()
                    && !matches!(bytes[index - 1], b':' | b'/' | b'>')))
        {
            return true;
        }
    }
    false
}

fn is_drive_absolute(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

fn is_staging_output(output: &Path) -> bool {
    if output
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return false;
    }
    let components: Vec<_> = output
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    components.windows(2).any(|pair| {
        pair[0] == ".comet-provider-captures" && pair[1].eq_ignore_ascii_case("staging")
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::{PublicationLock, publish_staging_pair_with, publish_staging_pair_with_commit};

    /// Break caught: writing events directly into the destination before manifest creation can
    /// leave a mixed or half-written pair and destroy a previously reviewable staging artifact.
    #[test]
    fn staging_pair_publish_preserves_existing_destination_on_second_write_failure() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join(".comet-provider-captures/staging");
        let destination = parent.join("scenario");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("events.jsonl"), b"old events").unwrap();
        std::fs::write(destination.join("manifest.json"), b"old manifest").unwrap();

        let error = publish_staging_pair_with(&destination, b"new events", b"new manifest", |_| {
            Err(std::io::Error::other("injected second-write failure"))
        })
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(
            std::fs::read(destination.join("events.jsonl")).unwrap(),
            b"old events"
        );
        assert_eq!(
            std::fs::read(destination.join("manifest.json")).unwrap(),
            b"old manifest"
        );
        let siblings: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(siblings, [std::ffi::OsString::from("scenario")]);
    }

    /// Break caught: backup-and-replace publication mutates reviewed evidence when rerun bytes
    /// differ, and a failed rollback can then lose the original pair.
    #[test]
    fn staging_pair_publish_rejects_a_different_existing_destination() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join(".comet-provider-captures/staging");
        let destination = parent.join("scenario");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("events.jsonl"), b"reviewed events").unwrap();
        std::fs::write(destination.join("manifest.json"), b"reviewed manifest").unwrap();

        let error = publish_staging_pair_with(
            &destination,
            b"different events",
            b"different manifest",
            |_| Ok(()),
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(destination.join("events.jsonl")).unwrap(),
            b"reviewed events"
        );
        assert_eq!(
            std::fs::read(destination.join("manifest.json")).unwrap(),
            b"reviewed manifest"
        );
        let siblings: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(siblings, [std::ffi::OsString::from("scenario")]);
    }

    /// Break caught: an identical rerun still swaps directories, violating immutable destination
    /// semantics and exposing a needless Windows rename failure path.
    #[test]
    fn staging_pair_publish_identical_existing_destination_is_a_noop() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join(".comet-provider-captures/staging");
        let destination = parent.join("scenario");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("events.jsonl"), b"same events").unwrap();
        std::fs::write(destination.join("manifest.json"), b"same manifest").unwrap();

        publish_staging_pair_with_commit(
            &destination,
            b"same events",
            b"same manifest",
            |_| Ok(()),
            |_, _| panic!("identical evidence must not invoke the commit rename"),
        )
        .unwrap();

        assert_eq!(
            std::fs::read(destination.join("events.jsonl")).unwrap(),
            b"same events"
        );
        assert_eq!(
            std::fs::read(destination.join("manifest.json")).unwrap(),
            b"same manifest"
        );
        let siblings: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(siblings, [std::ffi::OsString::from("scenario")]);
    }

    /// Break caught: comparing only the expected file bytes accepts a destination containing
    /// unreviewed extra entries as though it were the exact immutable evidence pair.
    #[test]
    fn staging_pair_publish_rejects_identical_pair_with_extra_entry() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join(".comet-provider-captures/staging");
        let destination = parent.join("scenario");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("events.jsonl"), b"same events").unwrap();
        std::fs::write(destination.join("manifest.json"), b"same manifest").unwrap();
        std::fs::write(destination.join("unreviewed.txt"), b"must remain untouched").unwrap();

        let error =
            publish_staging_pair_with(&destination, b"same events", b"same manifest", |_| Ok(()))
                .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(destination.join("events.jsonl")).unwrap(),
            b"same events"
        );
        assert_eq!(
            std::fs::read(destination.join("manifest.json")).unwrap(),
            b"same manifest"
        );
        assert_eq!(
            std::fs::read(destination.join("unreviewed.txt")).unwrap(),
            b"must remain untouched"
        );
        let siblings: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(siblings, [std::ffi::OsString::from("scenario")]);
    }

    /// Break caught: a failed final rename can leave a generated sibling or a partially visible
    /// destination, especially because directory replacement differs across platforms.
    #[test]
    fn staging_pair_publish_cleans_up_after_final_rename_failure() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join(".comet-provider-captures/staging");
        let destination = parent.join("scenario");

        let error = publish_staging_pair_with_commit(
            &destination,
            b"complete events",
            b"complete manifest",
            |_| Ok(()),
            |_, _| Err(std::io::Error::other("injected final rename failure")),
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(parent).unwrap().count(), 0);
    }

    /// Break caught: checking destination existence without serializing publishers lets a second
    /// sanitizer enter preparation and publish while the first is paused before commit.
    #[test]
    fn staging_pair_publish_serializes_concurrent_publishers() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join(".comet-provider-captures/staging");
        let destination = parent.join("scenario");
        let (first_entered_tx, first_entered_rx) = mpsc::channel();
        let (release_first_tx, release_first_rx) = mpsc::channel();
        let first_destination = destination.clone();
        let first = thread::spawn(move || {
            publish_staging_pair_with(
                &first_destination,
                b"concurrent events",
                b"concurrent manifest",
                |_| {
                    first_entered_tx.send(()).unwrap();
                    release_first_rx.recv().unwrap();
                    Ok(())
                },
            )
        });
        first_entered_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();

        let second_hook_ran = Arc::new(AtomicBool::new(false));
        let second_hook_flag = Arc::clone(&second_hook_ran);
        let second_destination = destination.clone();
        let (second_done_tx, second_done_rx) = mpsc::channel();
        let second = thread::spawn(move || {
            let result = publish_staging_pair_with(
                &second_destination,
                b"concurrent events",
                b"concurrent manifest",
                |_| {
                    second_hook_flag.store(true, Ordering::SeqCst);
                    Ok(())
                },
            );
            second_done_tx.send(result).unwrap();
        });
        let second_result = second_done_rx.recv_timeout(Duration::from_secs(2));
        release_first_tx.send(()).unwrap();
        let first_result = first.join().unwrap();
        second.join().unwrap();

        let second_error = second_result
            .expect("a contending publisher must fail without waiting")
            .unwrap_err();
        assert!(matches!(
            second_error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::AlreadyExists
        ));
        assert!(!second_error.to_string().contains("scenario"));
        assert!(!second_hook_ran.load(Ordering::SeqCst));
        first_result.unwrap();
        assert_eq!(
            std::fs::read(destination.join("events.jsonl")).unwrap(),
            b"concurrent events"
        );
        assert_eq!(
            std::fs::read(destination.join("manifest.json")).unwrap(),
            b"concurrent manifest"
        );
        let siblings: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(siblings, [std::ffi::OsString::from("scenario")]);
    }

    /// Break caught: cleanup keyed only by lock pathname can delete a replacement lock that the
    /// publisher did not acquire, stealing ownership from a non-cooperating actor.
    #[test]
    fn publication_lock_cleanup_refuses_changed_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join(".comet-provider-captures/staging");
        std::fs::create_dir_all(&parent).unwrap();
        let lock = PublicationLock::acquire(&parent, "scenario").unwrap();
        let path = parent.join(".scenario.publish.lock");
        std::fs::write(&path, b"replacement owner").unwrap();

        let error = lock.release().unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read(path).unwrap(), b"replacement owner");
    }

    use std::path::PathBuf;

    use serde_json::{Value, json};

    use super::{Redactor, SanitizationError, SanitizationReport};
    use crate::capture::types::Provider;

    /// Runs one JSON value through the allowlist redactor without touching the
    /// filesystem, for the `Ok` case. Panics on the credential-rejection case;
    /// use `try_sanitize_value` there.
    fn sanitize_value(value: Value, provider: Provider) -> Value {
        try_sanitize_value(value, provider).expect("test value expected to sanitize cleanly")
    }

    /// Same as `sanitize_value`, but hands back the `Result` so a test can
    /// assert on the rejection path (a credential riding an allowlisted path).
    fn try_sanitize_value(value: Value, provider: Provider) -> Result<Value, SanitizationError> {
        let mut value = value;
        let mut redactor = Redactor::default();
        redactor.sanitize_value_tree(&mut value, provider, "", "", "value")?;
        Ok(value)
    }

    /// Same walk as `sanitize_value`, but returns a `SanitizationReport` —
    /// `events_bytes` holds the single sanitized value and `manifest_bytes`
    /// the placeholder/count accounting, so Task 3's novel-path report can
    /// exercise this without a full `sanitize_dir` round trip.
    fn sanitize_value_reporting(value: Value, provider: Provider) -> SanitizationReport {
        let mut value = value;
        let mut redactor = Redactor::default();
        redactor
            .sanitize_value_tree(&mut value, provider, "", "", "value")
            .expect("test value expected to sanitize cleanly");
        let events_bytes = serde_json::to_vec(&value).expect("sanitized value encodes");
        let manifest_bytes = serde_json::to_vec(&json!({
            "placeholders": redactor.placeholder_definitions(),
            "redaction_counts": redactor.counts,
        }))
        .expect("manifest encodes");
        SanitizationReport {
            events_path: PathBuf::new(),
            manifest_path: PathBuf::new(),
            events_bytes,
            manifest_bytes,
        }
    }

    /// Exercises `sanitize_value_reporting` so it is not dead code before Task 3
    /// gives it a real caller; also documents its filesystem-free contract.
    #[test]
    fn sanitize_value_reporting_produces_a_report_without_touching_the_filesystem() {
        let report = sanitize_value_reporting(json!({"method": "turn/completed"}), Provider::Codex);
        assert!(!report.events_bytes.is_empty());
        assert!(!report.manifest_bytes.is_empty());
    }

    /// Equal values share a placeholder number, so a join that was true on the
    /// wire is still true in the archive. This is the property that lets the
    /// taxonomy go: identity comes from the value, not from a kind.
    #[test]
    fn equal_values_share_a_placeholder_and_distinct_values_do_not() {
        let out = sanitize_value(
            json!({
                "sessionId": "abc", "echo": "abc", "other": "xyz"
            }),
            Provider::Claude,
        );
        assert_eq!(out["sessionId"], out["echo"]);
        assert_ne!(out["sessionId"], out["other"]);
    }

    /// A field nobody has considered arrives redacted, not verbatim. This is
    /// the whole difference from the blocklist.
    #[test]
    fn an_unlisted_field_is_replaced_by_default() {
        let out = sanitize_value(json!({"somethingBrandNew": "secret-ish"}), Provider::Claude);
        assert_ne!(out["somethingBrandNew"], "secret-ish");
    }

    /// An allowlisted value survives byte-for-byte.
    #[test]
    fn a_listed_field_survives_verbatim() {
        let out = sanitize_value(json!({"method": "turn/completed"}), Provider::Codex);
        assert_eq!(out["method"], "turn/completed");
    }

    /// Six kinds read by name; everything else numbered.
    #[test]
    fn identifiers_are_named_and_everything_else_is_numbered() {
        let out = sanitize_value(json!({"sessionId": "s", "costUSD": "c"}), Provider::Claude);
        assert_eq!(out["sessionId"], "<SESSION_1>");
        assert_eq!(out["costUSD"], "<V1>");
    }

    /// An allowlisted path is still not a licence to publish a credential.
    #[test]
    fn a_credential_in_an_allowlisted_value_still_rejects() {
        let err = try_sanitize_value(json!({"method": "sk-ant-live-key"}), Provider::Codex);
        assert!(
            err.is_err(),
            "an allowlisted path carrying a token must reject"
        );
    }

    /// The tool-name-at-invocation family (`.request.tool_name` among them,
    /// per `crates/harness/src/capture/allowlist/claude.txt`) holds a
    /// built-in tool name on every capture in the corpus today, but an MCP
    /// invocation puts `mcp__<server>__<tool>` there instead — embedding the
    /// same server identity `.mcp_servers[].name` and `.tools[]` were both
    /// excluded to protect. A path decision cannot fix this: the path is
    /// legitimately allowlisted for the `Bash`/`Read`/… case. This is a
    /// value-level exception layered on top of the allowlist.
    #[test]
    fn an_mcp_tool_name_is_redacted_even_on_an_allowlisted_path() {
        let out = sanitize_value(
            json!({"request": {"tool_name": "mcp__claude_ai_Gmail__search_threads"}}),
            Provider::Claude,
        );
        assert_ne!(
            out["request"]["tool_name"],
            "mcp__claude_ai_Gmail__search_threads"
        );
    }

    /// The same allowlisted path with a built-in tool name is untouched —
    /// the exception is scoped to the `mcp__` prefix, not the whole path.
    #[test]
    fn a_builtin_tool_name_on_the_same_path_survives() {
        let out = sanitize_value(json!({"request": {"tool_name": "Bash"}}), Provider::Claude);
        assert_eq!(out["request"]["tool_name"], "Bash");
    }
}
