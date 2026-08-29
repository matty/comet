//! SNAPSHOT — the capability sheet (design §3.5).
//!
//! `render_sheet` is a pure function: evidence in, one markdown document out.
//! No filesystem, no `std::env`, no opinion about where the bytes end up —
//! the golden test in `capture_corpus/capability_sheets.rs` walks the
//! archive with [`super::surface::observe_surface`], reads each scenario's
//! `manifest.json` for its argv, working directory and configured
//! environment, and hands the result here. This module never touches either.
//!
//! **Line endings are `\n` only, deliberately.** `git show HEAD:AGENTS.md`
//! (and every other committed text file checked) stores LF; the CRLF a
//! Windows checkout shows is `core.autocrlf` converting on checkout, not
//! what is committed. CI runs on `ubuntu-24.04`, where no such conversion
//! happens. A generator that emitted `\r\n` would match the working tree on
//! a Windows checkout and mismatch the same golden file byte-for-byte on
//! Linux CI. Nothing here uses `writeln!`'s platform newline or a raw
//! `\r\n`; every line is joined with a literal `"\n"`.

use std::collections::{BTreeMap, BTreeSet};

use super::surface::{Direction, FieldObservation, VOCABULARY_PATHS};

/// One promoted scenario's identity, for the sheet's own evidence list.
///
/// Printing the exact argv (plan preamble decision 7, corrected 2026-08-16
/// after Task 3's review checked the manifests) still earns its place after
/// D80's resolution: it is what lets a reader confirm two scenarios were
/// launched the same way, or spot the one that wasn't, without going to the
/// manifest. D80 itself found that Claude's `model-discovery` and its former
/// `-neutral-cwd`/`-project-cwd` siblings were one observation recorded
/// under three names — a 2026-08-16 re-capture showed all three replies
/// differing only by a redacted `pid`, and Codex's three equivalents
/// byte-identical — so the two siblings are gone from the table (see
/// `docs/debt/closed.md` D80) and this sheet now prints `model-discovery`
/// once, not three times.
///
/// `cwd` and `configured_env` joined the argv (review finding, 2026-08-16):
/// a Claude-focused read of an earlier draft missed that argv alone is not
/// the whole launch. Codex's `model-discovery` and `model-discovery-logged-out`
/// share byte-identical argv but not `configured_env`: both set `CODEX_HOME`,
/// while `fresh-text`, `resume` and `steer` set nothing — a real difference
/// the argv-only block hid completely, while the sheet's own prose claimed
/// same-argv scenarios were launched identically.
#[derive(Clone, Debug)]
pub struct SheetScenario {
    pub name: String,
    pub purpose: String,
    /// `command.program` followed by `command.args`, verbatim from the
    /// manifest — including whatever redaction placeholder the archive
    /// already put there (`<CWD>`, `<HOME>`, `<REDACTED_1>`, …). Rendering
    /// starts at `argv[0]`; there is no separate program field.
    pub argv: Vec<String>,
    /// `command.cwd`, verbatim from the manifest (redacted to `<CWD>` in
    /// every scenario observed so far, same as every sibling — carried
    /// anyway so it prints beside the environment rather than being the one
    /// field a reader has to go check the manifest for).
    pub cwd: String,
    /// `command.configured_env`, verbatim from the manifest, sorted by key.
    /// Distinct from `argv`: two scenarios can share byte-identical argv and
    /// still not have been launched identically, which is exactly the case
    /// `CODEX_HOME` catches for Codex's `model-discovery*` family.
    pub configured_env: BTreeMap<String, String>,
    /// The length of a `system`/`init` frame's `tools` array, if this
    /// scenario's archive holds one — `None` when no such frame appears (a
    /// discovery-only scenario, or a provider that has no equivalent frame
    /// at all).
    ///
    /// Closes D86: the array itself is redacted element-by-element (`.tools`
    /// is not on `claude.txt`), so the Fields section below can only ever
    /// print the bare path, never a name from it — but the *length*
    /// survives redaction and is exactly what would show a roster change
    /// (a new built-in tool, or the recording account gaining or losing an
    /// MCP connector) that the Fields section is structurally blind to.
    /// Sourced from the archive's array length, never from any tool name
    /// inside it — this field must never carry anything that came out of a
    /// redacted element.
    pub tool_count: Option<usize>,
}

/// Renders one version's capability sheet as markdown.
///
/// `provider` and `version` name the sheet in its header and select which
/// observations count: an entry in `observations` whose own `provider` or
/// `version` does not match is excluded rather than trusted, so a caller
/// that accidentally hands this function the whole corpus instead of one
/// version's slice cannot produce a sheet that reports the wrong evidence
/// under the right name.
///
/// `vocabulary` and `scenarios` carry no such fields to check against, so
/// **the caller must already have scoped them to this `(provider, version)`
/// pair** — [`super::surface::Vocabulary`] is keyed by `(provider, version,
/// direction)`; the caller selects the two direction entries for this
/// version and remaps them to `(Direction, path)` before calling.
///
/// Same input in any order produces identical output: nothing here trusts
/// the caller's slice order for anything that is supposed to read sorted.
pub fn render_sheet(
    provider: &str,
    version: &str,
    observations: &[FieldObservation],
    vocabulary: &BTreeMap<(Direction, String), BTreeSet<String>>,
    scenarios: &[SheetScenario],
) -> String {
    let mut lines = Vec::new();
    lines.extend(header_lines(provider, version));
    lines.extend(scenario_lines(scenarios));
    lines.extend(field_lines(provider, version, observations));
    lines.extend(vocabulary_lines(vocabulary));

    // Every section above ends on a blank-line separator; trim the
    // accumulated trailing ones so the file ends in exactly one newline
    // rather than however many blank sections happened to run last.
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let mut rendered = lines.join("\n");
    rendered.push('\n');
    rendered
}

fn header_lines(provider: &str, version: &str) -> Vec<String> {
    vec![
        format!("# {provider} {version}"),
        String::new(),
        "Generated from the committed capture corpus — never from a live CLI, never from \
         what the scenario table merely declares. Regenerate with `$env:COMET_UPDATE_SHEETS \
         = \"1\"; cargo test -p comet-capture --test capture_corpus`; do not hand-edit."
            .to_owned(),
        String::new(),
        "This file reports only what the scenarios below actually produced. Diffing this \
         sheet against another version's sheet is the version-change report (no differ is \
         planned)."
            .to_owned(),
        String::new(),
        "Two readings that argv makes tempting and both wrong: identical launch flags do \
         not mean identical coverage — a frame or reply that depends on something actually \
         happening during the run only appears when a run produced that trigger, so the \
         same flag present in both versions' scenarios is not evidence the underlying \
         event fired in both. And a field or value that is new in one version is not \
         necessarily a new capability — it can be account or environment state that simply \
         did not happen to occur during the other version's runs, not a wire-format \
         change. Argv, cwd, env and scenario names narrow what to check; they do not \
         settle it on their own."
            .to_owned(),
        String::new(),
    ]
}

fn scenario_lines(scenarios: &[SheetScenario]) -> Vec<String> {
    let mut lines = vec![
        "## Scenarios".to_owned(),
        String::new(),
        "Every scenario this sheet's evidence is drawn from: the exact argv, working \
         directory and configured environment Comet launched it with (redaction \
         placeholders are the archive's, not this sheet's). A capability no scenario here \
         exercises cannot appear in the sections below, whatever the wire format might \
         otherwise support — this list is what makes that limit visible instead of silent. \
         A distinct name is not proof of distinct coverage, either, and a matching argv is \
         not proof of an identical launch: two scenarios can print the same argv and still \
         set different environment variables, and a placeholder's presence in one \
         scenario's env line and its absence from another's is real evidence a claim of \
         identical launches must survive. Compare the whole block — argv, cwd and env \
         together — before concluding two scenarios were launched identically, and compare \
         it again before concluding two with the same purpose sentence tested the same \
         thing — trusting the name or the purpose alone is not enough either way. Even a \
         whole-block comparison is not a sufficiency test, though: a redaction placeholder \
         cannot separate two scenarios whose real value redacted to the same token, so two \
         blocks that read byte-identical in every field can still have been launched with \
         genuinely different values underneath — the archive's placeholders prove a \
         difference when they show one, never that there wasn't one when they don't."
            .to_owned(),
        String::new(),
    ];

    let mut sorted: Vec<&SheetScenario> = scenarios.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));

    if sorted.is_empty() {
        lines.push("(no scenarios)".to_owned());
        lines.push(String::new());
        return lines;
    }

    for scenario in sorted {
        lines.push(format!("### {}", scenario.name));
        lines.push(String::new());
        lines.push(scenario.purpose.clone());
        lines.push(String::new());
        lines.push(format!("cwd: `{}`", scenario.cwd));
        lines.push(env_line(&scenario.configured_env));
        lines.push(tools_line(scenario.tool_count));
        lines.push(String::new());
        lines.push("```".to_owned());
        for arg in &scenario.argv {
            lines.push(arg.clone());
        }
        lines.push("```".to_owned());
        lines.push(String::new());
    }
    lines
}

/// `env: (none set)` when `configured_env` is empty (every Claude scenario in
/// the committed corpus), or `env: `KEY=value`, `KEY2=value2`` sorted by key
/// (`BTreeMap` iterates sorted already) — one line either way, so a reader
/// scanning the Scenarios section sees "nothing set" and "one variable set"
/// as two visibly different lines rather than an absent one meaning nothing.
fn env_line(configured_env: &BTreeMap<String, String>) -> String {
    if configured_env.is_empty() {
        return "env: (none set)".to_owned();
    }
    let vars: Vec<String> = configured_env
        .iter()
        .map(|(key, value)| format!("`{key}={value}`"))
        .collect();
    format!("env: {}", vars.join(", "))
}

/// `tools: 29` when this scenario's archive holds a `system`/`init` frame,
/// or `tools: (not observed)` when it does not — one line either way, the
/// same "an explicit line beats a missing one" shape [`env_line`] already
/// uses. This is the one place the array's *length* crosses from evidence
/// into rendered text; nothing here ever sees a tool name (D86).
fn tools_line(tool_count: Option<usize>) -> String {
    match tool_count {
        Some(count) => format!("tools: {count}"),
        None => "tools: (not observed)".to_owned(),
    }
}

fn field_lines(provider: &str, version: &str, observations: &[FieldObservation]) -> Vec<String> {
    let mut lines = vec![
        "## Fields".to_owned(),
        String::new(),
        "Every dotted path observed on the wire for this provider and version, split by the \
         direction it travelled — `To provider` is what Comet sends, `From provider` is \
         what the provider sends back — one path per line, sorted, each tagged with the \
         scenario group (below) that produced it. A field missing from this version's list \
         is only evidence the CLI dropped it if the scenarios that group names are also \
         present in the other version's own Scenarios section — a group made only of \
         scenarios this version's Scenarios section doesn't have means the field was simply \
         never exercised here, not removed."
            .to_owned(),
        String::new(),
    ];

    let scoped: Vec<&FieldObservation> = observations
        .iter()
        .filter(|observation| observation.provider == provider && observation.version == version)
        .collect();

    if scoped.is_empty() {
        for heading in ["To provider", "From provider"] {
            lines.push(format!("### {heading}"));
            lines.push(String::new());
            lines.push("(none observed)".to_owned());
            lines.push(String::new());
        }
        return lines;
    }

    let group_ids = scenario_group_ids(&scoped);
    lines.extend(scenario_group_lines(&group_ids));

    for (heading, direction) in [
        ("To provider", Direction::ToProvider),
        ("From provider", Direction::FromProvider),
    ] {
        lines.push(format!("### {heading}"));
        lines.push(String::new());

        let entries: BTreeMap<&str, usize> = scoped
            .iter()
            .filter(|observation| observation.direction == direction)
            .map(|observation| (observation.path.as_str(), group_ids[&observation.scenarios]))
            .collect();

        if entries.is_empty() {
            lines.push("(none observed)".to_owned());
        } else {
            for (path, id) in &entries {
                lines.push(format!("- `{path}` `G{id}`"));
            }
        }
        lines.push(String::new());
    }
    lines
}

/// Assigns every distinct scenario set among `scoped` a stable `G<n>` id,
/// 1-based in the sorted order of the sets themselves (`BTreeSet<String>`'s
/// own `Ord` — lexicographic over the sorted members), never in the order
/// `scoped` happened to be walked — what keeps [`render_sheet`]'s
/// determinism property (same input in any order, identical bytes) holding
/// for this too.
fn scenario_group_ids(scoped: &[&FieldObservation]) -> BTreeMap<BTreeSet<String>, usize> {
    let distinct: BTreeSet<BTreeSet<String>> = scoped
        .iter()
        .map(|observation| observation.scenarios.clone())
        .collect();
    distinct
        .into_iter()
        .enumerate()
        .map(|(index, scenarios)| (scenarios, index + 1))
        .collect()
}

/// Renders the `### Scenario groups` index: one line per distinct scenario
/// set, in id order. This is what makes the per-field `` `G<n>` `` tags below
/// mean something without a reader holding every field's scenario list in
/// their head — two fields observed in the same five scenarios collapse to
/// one group line instead of five names repeated twice.
fn scenario_group_lines(group_ids: &BTreeMap<BTreeSet<String>, usize>) -> Vec<String> {
    let mut lines = vec!["### Scenario groups".to_owned(), String::new()];
    let mut by_id: Vec<(usize, &BTreeSet<String>)> = group_ids
        .iter()
        .map(|(scenarios, id)| (*id, scenarios))
        .collect();
    by_id.sort_by_key(|(id, _)| *id);
    for (id, scenarios) in by_id {
        let names: Vec<&str> = scenarios.iter().map(String::as_str).collect();
        lines.push(format!("- `G{id}`: {}", names.join(", ")));
    }
    lines.push(String::new());
    lines
}

fn vocabulary_lines(vocabulary: &BTreeMap<(Direction, String), BTreeSet<String>>) -> Vec<String> {
    let mut lines = vec![
        "## Vocabulary".to_owned(),
        String::new(),
        "The observed value set for a small declared list of discriminator paths — not \
         every field, only the ones whose values name what kind of thing a frame or a tool \
         call is (`VOCABULARY_PATHS` in `crates/capture/src/surface.rs`). Every \
         path that const declares is listed under every direction, whether or not this \
         version's scenarios put a scalar there. `(none observed)` means exactly that: no \
         captured frame produced a value at that path in that direction, in this version's \
         evidence — it is not a claim that the provider lacks the capability. \
         Direction-keying itself is not a formality: a discriminator can carry a genuinely \
         different vocabulary per direction, not merely an unevenly observed one — the \
         value set one direction shows is not a subset of the other's, and a value native \
         to one direction may never appear in the other at all. Reading a path's values \
         without checking which direction produced them would silently merge two different \
         discriminators into one."
            .to_owned(),
        String::new(),
    ];

    let mut declared: Vec<&str> = VOCABULARY_PATHS.to_vec();
    declared.sort_unstable();

    for (heading, direction) in [
        ("To provider", Direction::ToProvider),
        ("From provider", Direction::FromProvider),
    ] {
        lines.push(format!("### {heading}"));
        lines.push(String::new());

        for path in &declared {
            lines.push(format!("#### `{path}`"));
            lines.push(String::new());
            match vocabulary.get(&(direction, (*path).to_string())) {
                Some(values) if !values.is_empty() => {
                    for value in values {
                        lines.push(format!("- `{value}`"));
                    }
                }
                _ => lines.push("(none observed)".to_owned()),
            }
            lines.push(String::new());
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::super::surface::FrameRef;
    use super::*;

    fn observation(
        provider: &str,
        version: &str,
        direction: Direction,
        path: &str,
    ) -> FieldObservation {
        observation_in(provider, version, direction, path, &["test"])
    }

    /// Like [`observation`], but with an explicit scenario set — what the
    /// scenario-group tests below need to control which fields land in the
    /// same `G<n>` group and which don't.
    fn observation_in(
        provider: &str,
        version: &str,
        direction: Direction,
        path: &str,
        scenarios: &[&str],
    ) -> FieldObservation {
        FieldObservation {
            provider: provider.to_owned(),
            version: version.to_owned(),
            path: path.to_owned(),
            direction,
            first_seen: FrameRef {
                scenario: format!(
                    "{provider}/{version}/{}",
                    scenarios.first().copied().unwrap_or("test")
                ),
                sequence: 1,
            },
            scenarios: scenarios.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    fn scenario(name: &str, purpose: &str, argv: &[&str]) -> SheetScenario {
        scenario_with_env(name, purpose, argv, "<CWD>", &[])
    }

    fn scenario_with_env(
        name: &str,
        purpose: &str,
        argv: &[&str],
        cwd: &str,
        env: &[(&str, &str)],
    ) -> SheetScenario {
        scenario_with_tools(name, purpose, argv, cwd, env, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn scenario_with_tools(
        name: &str,
        purpose: &str,
        argv: &[&str],
        cwd: &str,
        env: &[(&str, &str)],
        tool_count: Option<usize>,
    ) -> SheetScenario {
        SheetScenario {
            name: name.to_owned(),
            purpose: purpose.to_owned(),
            argv: argv.iter().map(|arg| arg.to_string()).collect(),
            cwd: cwd.to_owned(),
            configured_env: env
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
            tool_count,
        }
    }

    /// The text of one `##`-level section, from its own heading up to (but
    /// not including) the next `##`-level heading, or the end of the
    /// document for the last section.
    ///
    /// Exists so a test can assert against exactly the section it names
    /// rather than the whole rendered document. Two earlier versions of the
    /// tests below asserted `rendered.contains("(none observed)")` against
    /// the whole document while also passing empty input to the *other*
    /// section — Fields' two empty subsections satisfied the vocabulary
    /// test, and Vocabulary's sixteen empty subsections satisfied the
    /// fields test, regardless of what the section under test actually did.
    /// Review finding, 2026-08-16.
    ///
    /// Looks for `"\n\n## "` (blank separator, then the heading marker),
    /// not bare `"\n## "` — every genuine `##`-heading in `render_sheet`'s
    /// output is preceded by a blank line (the join always inserts one), so
    /// this can't be fooled by a scenario argv line that happens to start
    /// with `## ` inside a fenced block, which is preceded by a single `\n`.
    fn section<'a>(rendered: &'a str, heading: &str) -> &'a str {
        let start = rendered
            .find(heading)
            .unwrap_or_else(|| panic!("no {heading:?} heading in: {rendered}"));
        let after = &rendered[start..];
        let end = after[heading.len()..]
            .find("\n\n## ")
            .map(|offset| offset + heading.len() + 1)
            .unwrap_or(after.len());
        &after[..end]
    }

    /// Every `#### \`path\`` heading inside `section` must be followed
    /// (heading, blank separator, content) by exactly the text
    /// `(none observed)` — stronger than "the phrase appears somewhere in
    /// this section," which a real value at some *other* declared path
    /// could also satisfy while the path under test silently renders
    /// something else. Panics (rather than returning a count) if `section`
    /// names no heading at all, since that would make the loop below
    /// vacuously true.
    fn assert_every_heading_reads_none_observed(section: &str) {
        let mut checked = 0;
        for heading_start in section
            .match_indices("#### `")
            .map(|(index, _)| index)
            .collect::<Vec<_>>()
        {
            let after = &section[heading_start..];
            let heading_end = after
                .find('\n')
                .expect("heading line must end in a newline");
            let heading = &after[..heading_end];
            let rest = after[heading_end..]
                .strip_prefix("\n\n")
                .unwrap_or_else(|| {
                    panic!("{heading:?} must be followed by a blank separator line: {section}")
                });
            let content_end = rest.find('\n').unwrap_or(rest.len());
            let content = &rest[..content_end];
            assert_eq!(
                content, "(none observed)",
                "{heading} must read exactly \"(none observed)\", found {content:?}: {section}"
            );
            checked += 1;
        }
        assert!(checked > 0, "no declared-path headings found: {section}");
    }

    #[test]
    fn header_names_provider_and_version() {
        let rendered = render_sheet("claude", "2.1.229", &[], &BTreeMap::new(), &[]);
        assert!(rendered.starts_with("# claude 2.1.229\n"), "{rendered}");
    }

    /// The property the brief names explicitly: same input in any order
    /// must render identical bytes. This is what makes the golden test in
    /// Task 4 trustworthy at all — if the renderer trusted caller order,
    /// the walk order of the corpus (a filesystem detail) would leak into
    /// committed evidence.
    #[test]
    fn render_sheet_is_deterministic_regardless_of_input_order() {
        let observations_a = vec![
            observation("claude", "2.1.229", Direction::FromProvider, ".zeta"),
            observation("claude", "2.1.229", Direction::FromProvider, ".alpha"),
        ];
        let observations_b = vec![
            observation("claude", "2.1.229", Direction::FromProvider, ".alpha"),
            observation("claude", "2.1.229", Direction::FromProvider, ".zeta"),
        ];
        let scenarios_a = vec![
            scenario("zulu", "z purpose", &["prog", "--flag"]),
            scenario("alpha", "a purpose", &["prog"]),
        ];
        let scenarios_b = vec![
            scenario("alpha", "a purpose", &["prog"]),
            scenario("zulu", "z purpose", &["prog", "--flag"]),
        ];
        let vocabulary: BTreeMap<(Direction, String), BTreeSet<String>> = BTreeMap::from([(
            (Direction::FromProvider, ".type".to_owned()),
            BTreeSet::from(["assistant".to_owned(), "system".to_owned()]),
        )]);

        let rendered_a = render_sheet(
            "claude",
            "2.1.229",
            &observations_a,
            &vocabulary,
            &scenarios_a,
        );
        let rendered_b = render_sheet(
            "claude",
            "2.1.229",
            &observations_b,
            &vocabulary,
            &scenarios_b,
        );

        assert_eq!(
            rendered_a, rendered_b,
            "reordering the caller's inputs must not change the rendered bytes"
        );
    }

    /// The other property the brief names explicitly: an empty vocabulary
    /// must read as an explicit "none observed" against every declared
    /// path, not as a missing or shortened section — the two traps named in
    /// the task (a false version regression, and an empty section reading
    /// as an absence of capability) both hinge on this.
    ///
    /// Break caught (review, 2026-08-16): the original version of this test
    /// asserted `rendered.contains("(none observed)")` against the *whole*
    /// document while passing `&[]` for `observations` too — so `field_lines`
    /// emitted "(none observed)" twice before the Vocabulary section was
    /// even reached, and the assertion passed regardless of what the
    /// Vocabulary section actually rendered. Change `vocabulary_lines`'s
    /// `_ => lines.push("(none observed)".to_owned())` arm to
    /// `_ => lines.push(String::new())` and this fixed version still fails,
    /// which the original did not.
    #[test]
    fn an_empty_vocabulary_reads_as_none_observed_not_a_missing_section() {
        let rendered = render_sheet("codex", "0.147.0", &[], &BTreeMap::new(), &[]);
        let vocabulary_section = section(&rendered, "## Vocabulary");

        for path in VOCABULARY_PATHS {
            let heading = format!("#### `{path}`");
            assert!(
                vocabulary_section.contains(&heading),
                "declared path {path} must still get its own subsection with zero observed \
                 values: {vocabulary_section}"
            );
        }
        // Stronger than "the phrase appears somewhere in the section": every
        // one of the 8 declared paths x 2 directions = 16 headings must
        // individually read exactly "(none observed)", not merely have the
        // phrase appear once anywhere while some heading renders something
        // else (or nothing).
        assert_every_heading_reads_none_observed(vocabulary_section);
        assert!(
            vocabulary_section.contains("not a claim that the provider lacks the capability"),
            "the vocabulary section must explicitly deny that an unobserved path means the \
             provider lacks it: {vocabulary_section}"
        );
    }

    #[test]
    fn fields_are_sorted_within_a_direction() {
        let observations = vec![
            observation("claude", "2.1.229", Direction::FromProvider, ".zeta"),
            observation("claude", "2.1.229", Direction::FromProvider, ".alpha"),
            observation("claude", "2.1.229", Direction::FromProvider, ".mid"),
        ];
        let rendered = render_sheet("claude", "2.1.229", &observations, &BTreeMap::new(), &[]);

        let alpha = rendered.find("`.alpha`").unwrap();
        let mid = rendered.find("`.mid`").unwrap();
        let zeta = rendered.find("`.zeta`").unwrap();
        assert!(
            alpha < mid && mid < zeta,
            "fields must render sorted by path: {rendered}"
        );
    }

    /// D85: two fields observed in the exact same scenarios must share one
    /// `G<n>` tag, and a field observed in a different scenario set must get
    /// a different tag — the whole point of the group index is that it
    /// collapses repetition rather than printing a scenario list per field.
    ///
    /// Break this would catch: a naive per-field rendering (or a group
    /// keyed on something other than set equality, e.g. field insertion
    /// order) would either print `.shared_a` and `.shared_b`'s identical
    /// five-scenario list twice, or assign them different ids despite
    /// covering the same evidence.
    #[test]
    fn fields_with_the_same_scenario_set_share_one_group_tag() {
        let observations = vec![
            observation_in(
                "claude",
                "2.1.229",
                Direction::FromProvider,
                ".solo",
                &["alpha"],
            ),
            observation_in(
                "claude",
                "2.1.229",
                Direction::FromProvider,
                ".shared_a",
                &["alpha", "beta"],
            ),
            observation_in(
                "claude",
                "2.1.229",
                Direction::FromProvider,
                ".shared_b",
                &["alpha", "beta"],
            ),
        ];
        let rendered = render_sheet("claude", "2.1.229", &observations, &BTreeMap::new(), &[]);
        let fields_section = section(&rendered, "## Fields");

        assert!(
            fields_section.contains("### Scenario groups"),
            "{fields_section}"
        );
        let group_lines: Vec<&str> = fields_section
            .lines()
            .filter(|line| line.starts_with("- `G"))
            .collect();
        assert_eq!(
            group_lines.len(),
            2,
            "two distinct scenario sets ({{alpha}} and {{alpha, beta}}) must fold into two \
             group lines, not one per field: {group_lines:?}"
        );
        assert!(
            group_lines.contains(&"- `G1`: alpha"),
            "the solo scenario set must render as its own group: {group_lines:?}"
        );
        assert!(
            group_lines.contains(&"- `G2`: alpha, beta"),
            "the shared scenario set must render as its own group, sorted: {group_lines:?}"
        );

        let tag_for = |path: &str| -> &str {
            let marker = format!("`{path}` `G");
            let start = fields_section
                .find(&marker)
                .unwrap_or_else(|| panic!("{path} not tagged: {fields_section}"));
            let after = &fields_section[start + marker.len()..];
            let end = after.find('`').unwrap();
            &after[..end]
        };
        assert_eq!(
            tag_for(".shared_a"),
            tag_for(".shared_b"),
            "fields observed in the identical scenario set must carry the identical tag: \
             {fields_section}"
        );
        assert_ne!(
            tag_for(".solo"),
            tag_for(".shared_a"),
            "fields observed in different scenario sets must carry different tags: \
             {fields_section}"
        );
    }

    /// D85's actual reproduction case: two versions whose evidence comes
    /// from entirely disjoint scenarios must be distinguishable, from the
    /// diff alone, from two versions where the same scenario stopped
    /// producing a field. A field present under a group naming a scenario
    /// this version's own Scenarios section holds is a real signal; a field
    /// present under a group naming only scenarios foreign to this
    /// version's Scenarios section is not — this is the check a reader
    /// applies to tell the two apart without opening the corpus.
    #[test]
    fn a_fields_group_names_only_scenarios_this_version_actually_ran() {
        let observations = vec![observation_in(
            "claude",
            "2.1.229",
            Direction::FromProvider,
            ".turn_only",
            &["checklist", "subagent"],
        )];
        let scenarios = vec![
            scenario("checklist", "capture one bounded run", &["prog"]),
            scenario("checklist-resume", "capture a resumed run", &["prog"]),
            scenario("subagent", "capture a subagent run", &["prog"]),
        ];
        let rendered = render_sheet(
            "claude",
            "2.1.229",
            &observations,
            &BTreeMap::new(),
            &scenarios,
        );
        let fields_section = section(&rendered, "## Fields");
        let scenarios_section = section(&rendered, "## Scenarios");

        assert!(
            fields_section.contains("- `G1`: checklist, subagent"),
            "{fields_section}"
        );
        // Every scenario the group names is one this version's own
        // Scenarios section documents — the case where a diff reader can
        // conclude the field is real, not merely untested here.
        for name in ["checklist", "subagent"] {
            assert!(
                scenarios_section.contains(&format!("### {name}")),
                "the group names {name}, which must be one of this version's own \
                 scenarios: {scenarios_section}"
            );
        }
    }

    /// Break this would catch: `render_sheet` trusting every observation it
    /// is handed rather than filtering to the named `(provider, version)`
    /// would let a caller bug (e.g. accidentally passing the whole corpus)
    /// silently render evidence under the wrong header.
    #[test]
    fn observations_from_a_different_provider_or_version_are_excluded() {
        let observations = vec![
            observation(
                "claude",
                "2.1.229",
                Direction::FromProvider,
                ".this_version",
            ),
            observation(
                "claude",
                "2.1.228",
                Direction::FromProvider,
                ".other_version",
            ),
            observation(
                "codex",
                "2.1.229",
                Direction::FromProvider,
                ".other_provider",
            ),
        ];
        let rendered = render_sheet("claude", "2.1.229", &observations, &BTreeMap::new(), &[]);

        assert!(rendered.contains(".this_version"), "{rendered}");
        assert!(!rendered.contains(".other_version"), "{rendered}");
        assert!(!rendered.contains(".other_provider"), "{rendered}");
    }

    #[test]
    fn scenarios_render_sorted_with_verbatim_argv() {
        let scenarios = vec![
            scenario("zulu", "z purpose", &["<HOME>\\claude.exe", "--flag"]),
            scenario(
                "alpha",
                "a purpose",
                &["<HOME>\\claude.exe", "--model", "<REDACTED_1>"],
            ),
        ];
        let rendered = render_sheet("claude", "2.1.229", &[], &BTreeMap::new(), &scenarios);

        let alpha_index = rendered.find("### alpha").unwrap();
        let zulu_index = rendered.find("### zulu").unwrap();
        assert!(
            alpha_index < zulu_index,
            "scenarios must be sorted by name: {rendered}"
        );
        assert!(
            rendered.contains("<REDACTED_1>"),
            "argv placeholders must render verbatim: {rendered}"
        );
    }

    /// Break caught (Task 4 review, 2026-08-16): the Scenarios intro claimed
    /// "two scenarios below with the same argv were launched identically",
    /// and nothing printed could contradict it — a Claude-focused read
    /// missed that Codex's `model-discovery` and `fresh-text` share
    /// byte-identical argv (`app-server`) but not `configured_env`
    /// (`CODEX_HOME` set vs. nothing set). `cwd` and `configured_env` must
    /// render even when `argv` is identical across scenarios, and an empty
    /// env must read as an explicit "nothing set" rather than a blank or
    /// missing line — the same "none observed" rather than "no line at all"
    /// principle the Fields and Vocabulary sections already hold to.
    #[test]
    fn cwd_and_env_render_even_when_argv_is_identical() {
        let scenarios = vec![
            scenario_with_env(
                "fresh-text",
                "capture one bounded Codex run script",
                &["<HOME>\\codex.exe", "app-server"],
                "<CWD>",
                &[],
            ),
            scenario_with_env(
                "model-discovery",
                "capture Codex initialize and paged model/list replies",
                &["<HOME>\\codex.exe", "app-server"],
                "<CWD>",
                &[("CODEX_HOME", "<CODEX_HOME>")],
            ),
        ];
        let rendered = render_sheet("codex", "0.147.0", &[], &BTreeMap::new(), &scenarios);
        let scenarios_section = section(&rendered, "## Scenarios");

        let fresh_text = &scenarios_section[scenarios_section.find("### fresh-text").unwrap()
            ..scenarios_section.find("### model-discovery").unwrap()];
        let model_discovery =
            &scenarios_section[scenarios_section.find("### model-discovery").unwrap()..];

        assert!(
            fresh_text.contains("env: (none set)"),
            "a scenario with no configured_env must say so explicitly: {fresh_text}"
        );
        assert!(
            model_discovery.contains("env: `CODEX_HOME=<CODEX_HOME>`"),
            "a scenario's configured_env must render even though its argv is identical to a \
             sibling's: {model_discovery}"
        );
        assert!(
            fresh_text.contains("cwd: `<CWD>`") && model_discovery.contains("cwd: `<CWD>`"),
            "cwd must render for every scenario: {scenarios_section}"
        );
    }

    /// D86: a scenario whose archive holds a `system`/`init` frame's `tools`
    /// array renders its observed length, and a scenario with no such frame
    /// says so explicitly rather than omitting the line — the same
    /// present-vs-absent contrast [`cwd_and_env_render_even_when_argv_is_identical`]
    /// already proves for `env`.
    #[test]
    fn tools_line_renders_the_observed_length_or_says_not_observed() {
        let scenarios = vec![
            scenario_with_tools(
                "subagent",
                "capture a Claude subagent run",
                &["<HOME>\\claude.exe"],
                "<CWD>",
                &[],
                Some(35),
            ),
            scenario_with_tools(
                "model-discovery",
                "capture Claude model discovery",
                &["<HOME>\\claude.exe", "--bare"],
                "<CWD>",
                &[],
                None,
            ),
        ];
        let rendered = render_sheet("claude", "2.1.229", &[], &BTreeMap::new(), &scenarios);
        let scenarios_section = section(&rendered, "## Scenarios");

        let model_discovery =
            &scenarios_section[scenarios_section.find("### model-discovery").unwrap()
                ..scenarios_section.find("### subagent").unwrap()];
        let subagent = &scenarios_section[scenarios_section.find("### subagent").unwrap()..];

        assert!(
            subagent.contains("tools: 35"),
            "a scenario with an observed tools array must print its length: {subagent}"
        );
        assert!(
            model_discovery.contains("tools: (not observed)"),
            "a scenario with no system/init frame must say so explicitly, not omit the line: \
             {model_discovery}"
        );
    }

    /// Break caught (review, 2026-08-16): the original version of this test
    /// asserted `rendered.contains("(none observed)")` against the *whole*
    /// document while passing an empty `vocabulary` too — so
    /// `vocabulary_lines` emitted "(none observed)" sixteen times (8
    /// declared paths x 2 directions) regardless of what `field_lines`
    /// rendered, and the assertion passed no matter what. Change
    /// `field_lines`'s `lines.push("(none observed)".to_owned())` to
    /// `lines.push(String::new())` and this fixed version still fails,
    /// which the original did not.
    #[test]
    fn fields_with_nothing_observed_still_say_so() {
        let rendered = render_sheet("claude", "2.1.229", &[], &BTreeMap::new(), &[]);
        let fields_section = section(&rendered, "## Fields");

        let to_provider_idx = fields_section.find("### To provider").unwrap();
        let from_provider_idx = fields_section.find("### From provider").unwrap();
        assert!(to_provider_idx < from_provider_idx, "{fields_section}");

        for heading in ["### To provider", "### From provider"] {
            let after = &fields_section[fields_section.find(heading).unwrap() + heading.len()..];
            let content = after.strip_prefix("\n\n").unwrap_or_else(|| {
                panic!("{heading:?} must be followed by a blank separator line: {fields_section}")
            });
            let content_end = content.find('\n').unwrap_or(content.len());
            assert_eq!(
                &content[..content_end],
                "(none observed)",
                "{heading} must read exactly \"(none observed)\" when nothing was observed: \
                 {fields_section}"
            );
        }
    }

    #[test]
    fn output_uses_lf_line_endings_only() {
        let rendered = render_sheet("claude", "2.1.229", &[], &BTreeMap::new(), &[]);
        assert!(
            !rendered.contains('\r'),
            "generated markdown must be LF-only: {rendered:?}"
        );
        assert!(
            rendered.ends_with('\n') && !rendered.ends_with("\n\n"),
            "must end in exactly one trailing newline: {rendered:?}"
        );
    }

    /// Break caught (review, 2026-08-16): the Scenarios intro named Claude's
    /// `model-discovery` trio as a worked example of same-argv scenarios,
    /// and it rendered into *every* sheet regardless of `provider` — a
    /// Codex reader got a Claude anecdote while Codex's own, structurally
    /// identical case (its four `model-discovery*` scenarios share
    /// byte-identical program/args/cwd, verified directly against the
    /// corpus) went unmentioned. The header and the Vocabulary intro carried
    /// the same defect (`can_use_tool`, `rate_limit_event`,
    /// `.request.subtype`/`initialize` are Claude-only literals that used to
    /// render into the Codex sheet too).
    ///
    /// An earlier version of this test rendered the same synthetic evidence
    /// under two different `provider` values and asserted the output was
    /// identical past the header line — which sounds like it tests "no
    /// provider-specific literal," but doesn't: a hardcoded literal that
    /// renders unconditionally, the same way regardless of `provider` (which
    /// is exactly the shape the real bug had — the string wasn't gated on
    /// `if provider == "claude"`, it was just always there), satisfies an
    /// equality check trivially, because both sides of the diff contain it
    /// equally. Falsifying that version by reintroducing the literal proved
    /// this: the test stayed green. Replaced with a direct search for the
    /// literals the review actually found, which does discriminate — a
    /// Codex render must never contain a name that belongs only to Claude's
    /// evidence.
    #[test]
    fn a_codex_sheet_names_no_claude_only_literal() {
        // Evidence built entirely from generic, non-Claude, non-Codex
        // strings, so if any of the literals below show up in the output,
        // they can only have come from the renderer's own static prose.
        let observations = vec![observation(
            "codex",
            "9.9.9",
            Direction::FromProvider,
            ".shared",
        )];
        let vocabulary: BTreeMap<(Direction, String), BTreeSet<String>> = BTreeMap::from([(
            (Direction::FromProvider, ".type".to_owned()),
            BTreeSet::from(["shared-value".to_owned()]),
        )]);
        let scenarios = vec![scenario(
            "shared-scenario",
            "shared purpose",
            &["prog", "--flag"],
        )];

        let rendered = render_sheet("codex", "9.9.9", &observations, &vocabulary, &scenarios);

        // `.request.subtype` itself is deliberately excluded from this list:
        // it's a declared path name in `VOCABULARY_PATHS`, shared by every
        // provider, and correctly appears as one of Codex's own eight
        // Vocabulary subsections (reading "(none observed)", since Codex
        // never populates it). What must never appear are the concrete
        // *values* that path takes under Claude.
        //
        // Two of these five only work as Claude-only literals against THIS
        // synthetic evidence, which deliberately avoids both. Codex's own
        // real corpus is not Claude-only for either: `.method` is
        // `"initialize"` for Comet's own request on
        // `codex/0.147.0/model-discovery`'s first frame (`stdin`,
        // to-provider), so the real `codex-0.147.0.md` legitimately contains
        // that string in its Vocabulary section — and Codex has four of its
        // own `model-discovery*` scenario rows (`model-discovery`,
        // `-logged-out`, `-neutral-cwd`, `-project-cwd`), so the real sheet's
        // Scenarios section legitimately contains `"model-discovery"` too, as
        // a substring of all four of those names. Do not repoint this
        // test's `observations`/`vocabulary`/`scenarios` at the real corpus
        // without first dropping BOTH `"initialize"` and `"model-discovery"`
        // from this list — doing so would fail for a correct reason (real
        // Codex evidence) rather than the leak this test exists to catch.
        for literal in [
            "Claude",
            "model-discovery",
            "can_use_tool",
            "rate_limit_event",
            "initialize",
        ] {
            assert!(
                !rendered.contains(literal),
                "a Codex sheet must never name {literal:?} — that names a concrete frame, \
                 tool or scenario absent from Codex's own evidence: {rendered}"
            );
        }
    }
}
