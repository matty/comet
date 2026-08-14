//! Task 2 of C0: which wire fields does Comet actually name?
//!
//! Read from the decode modules' own AST rather than a regex. The prototype
//! that motivated this slice used a grep, counted any Rust field name anywhere,
//! and was wrong in both directions - it is why the 217-field figure in the
//! spec is labelled indicative.
//!
//! The set is deliberately narrow. A field Comet reads dynamically
//! (`input.get(key)`) cannot be seen here and will show as unknown, which is an
//! over-report: it costs triage, it does not hide a gap. Widening this to "any
//! string literal in the module" would fail the other way.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use syn::{Expr, Item, Lit, Meta, visit::Visit};

/// Every wire name the given sources mention: serde field names, variant
/// renames, and the literals of untyped reads.
pub(super) fn consumed_fields(sources: &[PathBuf]) -> Result<BTreeSet<String>, String> {
    let mut names = BTreeSet::new();
    for source in sources {
        let text = std::fs::read_to_string(source)
            .map_err(|error| format!("{} could not be read: {error}", source.display()))?;
        let file = syn::parse_file(&text)
            .map_err(|error| format!("{} could not be parsed: {error}", source.display()))?;
        let mut collector = Collector {
            names: &mut names,
            rename_all: None,
        };
        collector.visit_file(&file);
    }
    Ok(names)
}

/// Decode modules, in path order. `capture/` is the recording rig and `bin/` is
/// operator tooling; neither decodes provider replies for the product.
pub(super) fn decode_sources(crate_root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut sources = Vec::new();
    let mut stack = vec![crate_root.join("src")];
    while let Some(directory) = stack.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("{} could not be read: {error}", directory.display()))?;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if path.is_dir() {
                if name != "capture" && name != "bin" {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|extension| extension == "rs")
                && name != "capture.rs"
            {
                sources.push(path);
            }
        }
    }
    sources.sort();
    if sources.is_empty() {
        return Err(format!(
            "{} holds no decode sources",
            crate_root.join("src").display()
        ));
    }
    Ok(sources)
}

struct Collector<'a> {
    names: &'a mut BTreeSet<String>,
    /// The `rename_all` of the struct currently being walked.
    rename_all: Option<String>,
}

impl Collector<'_> {
    fn record(&mut self, name: String) {
        if !name.is_empty() {
            self.names.insert(name);
        }
    }
}

impl<'ast> Visit<'ast> for Collector<'_> {
    fn visit_item(&mut self, item: &'ast Item) {
        // A test naming a field is not Comet consuming it.
        if let Item::Mod(module) = item
            && module.attrs.iter().any(is_cfg_test)
        {
            return;
        }
        match item {
            Item::Struct(item) => {
                let outer = std::mem::replace(&mut self.rename_all, rename_all(&item.attrs));
                for field in &item.fields {
                    if skipped(&field.attrs) {
                        continue;
                    }
                    let name = match rename(&field.attrs) {
                        Some(rename) => rename,
                        None => {
                            let ident = field
                                .ident
                                .as_ref()
                                .map(ToString::to_string)
                                .unwrap_or_default();
                            apply_rename_all(&ident, self.rename_all.as_deref())
                        }
                    };
                    self.record(name);
                }
                self.rename_all = outer;
            }
            Item::Enum(item) => {
                let outer = std::mem::replace(&mut self.rename_all, rename_all(&item.attrs));
                for variant in &item.variants {
                    if let Some(rename) = rename(&variant.attrs) {
                        self.record(rename);
                    }
                    for field in &variant.fields {
                        if skipped(&field.attrs) {
                            continue;
                        }
                        let name = match rename(&field.attrs) {
                            Some(rename) => rename,
                            None => {
                                let ident = field
                                    .ident
                                    .as_ref()
                                    .map(ToString::to_string)
                                    .unwrap_or_default();
                                apply_rename_all(&ident, self.rename_all.as_deref())
                            }
                        };
                        self.record(name);
                    }
                }
                self.rename_all = outer;
            }
            _ => {}
        }
        syn::visit::visit_item(self, item);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            // `object.get("key")`
            Expr::MethodCall(call) if call.method == "get" && call.args.len() == 1 => {
                if let Some(literal) = string_literal(&call.args[0]) {
                    self.record(literal);
                }
            }
            // `object["key"]`
            Expr::Index(index) => {
                if let Some(literal) = string_literal(&index.index) {
                    self.record(literal);
                }
            }
            _ => {}
        }
        syn::visit::visit_expr(self, expr);
    }
}

fn string_literal(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Lit(literal) => match &literal.lit {
            Lit::Str(text) => Some(text.value()),
            _ => None,
        },
        Expr::Reference(reference) => string_literal(&reference.expr),
        _ => None,
    }
}

fn is_cfg_test(attribute: &syn::Attribute) -> bool {
    attribute.path().is_ident("cfg")
        && attribute
            .parse_args::<Meta>()
            .is_ok_and(|meta| meta.path().is_ident("test"))
}

fn serde_values(attributes: &[syn::Attribute], wanted: &str) -> Option<String> {
    for attribute in attributes {
        if !attribute.path().is_ident("serde") {
            continue;
        }
        let mut found = None;
        let _ = attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident(wanted)
                && let Ok(value) = meta.value()
                && let Ok(syn::Lit::Str(text)) = value.parse::<Lit>()
            {
                found = Some(text.value());
            }
            Ok(())
        });
        if found.is_some() {
            return found;
        }
    }
    None
}

fn rename(attributes: &[syn::Attribute]) -> Option<String> {
    serde_values(attributes, "rename")
}

fn rename_all(attributes: &[syn::Attribute]) -> Option<String> {
    serde_values(attributes, "rename_all")
}

fn skipped(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        if !attribute.path().is_ident("serde") {
            return false;
        }
        let mut skipped = false;
        let _ = attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip") || meta.path.is_ident("skip_deserializing") {
                skipped = true;
            }
            Ok(())
        });
        skipped
    })
}

fn apply_rename_all(ident: &str, rule: Option<&str>) -> String {
    let raw = ident.strip_prefix("r#").unwrap_or(ident);
    match rule {
        Some("camelCase") => {
            let mut parts = raw.split('_');
            let first = parts.next().unwrap_or_default().to_owned();
            parts.fold(first, |mut name, part| {
                let mut characters = part.chars();
                if let Some(first) = characters.next() {
                    name.push(first.to_ascii_uppercase());
                    name.push_str(characters.as_str());
                }
                name
            })
        }
        Some("PascalCase") => raw.split('_').fold(String::new(), |mut name, part| {
            let mut characters = part.chars();
            if let Some(first) = characters.next() {
                name.push(first.to_ascii_uppercase());
                name.push_str(characters.as_str());
            }
            name
        }),
        Some("kebab-case") => raw.replace('_', "-"),
        Some("SCREAMING_SNAKE_CASE") => raw.to_ascii_uppercase(),
        Some("lowercase") => raw.to_ascii_lowercase(),
        _ => raw.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> BTreeSet<String> {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fixture.rs");
        std::fs::write(&path, source).unwrap();
        consumed_fields(&[path]).unwrap()
    }

    /// Break caught: `rename_all` is ignored, so a `stop_reason` field reads as
    /// unconsumed while the wire says `stopReason` - and the report invents a
    /// gap that does not exist.
    #[test]
    fn field_names_apply_rename_and_rename_all() {
        let names = parse(
            r#"
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Reply {
                stop_reason: String,
                #[serde(rename = "isDefault")]
                default_flag: bool,
                #[serde(skip)]
                local_only: u8,
                plain: u8,
            }
            "#,
        );
        assert!(names.contains("stopReason"), "{names:?}");
        assert!(names.contains("isDefault"), "{names:?}");
        assert!(names.contains("plain"), "{names:?}");
        assert!(
            !names.contains("stop_reason"),
            "the snake_case ident is not what crosses the wire: {names:?}"
        );
        assert!(!names.contains("local_only"), "{names:?}");
    }

    /// Break caught: a test module's reads count as consumption, so a field
    /// only ever named by an assertion is reported as used by the product.
    #[test]
    fn a_cfg_test_module_is_not_consumption() {
        let names = parse(
            r#"
            struct Real { kept: u8 }

            #[cfg(test)]
            mod tests {
                struct OnlyInTests { never_shipped: u8 }
                fn probe(value: &Value) { let _ = value.get("test_only_key"); }
            }
            "#,
        );
        assert!(names.contains("kept"), "{names:?}");
        assert!(!names.contains("never_shipped"), "{names:?}");
        assert!(!names.contains("test_only_key"), "{names:?}");
    }

    /// Break caught: untyped reads are invisible, so every field Comet reads
    /// through `get("..")` rather than a struct shows as unknown.
    #[test]
    fn untyped_reads_are_collected_in_both_forms() {
        let names = parse(
            r#"
            fn decode(value: &Value) {
                let _ = value.get("tool_use_result");
                let _ = value["sequence"];
            }
            "#,
        );
        assert!(names.contains("tool_use_result"), "{names:?}");
        assert!(names.contains("sequence"), "{names:?}");
    }

    /// Break caught: an unreadable or misnamed source silently yields an empty
    /// set, every observed field reports as unconsumed, and the result reads as
    /// a dramatic finding instead of a broken scan.
    #[test]
    fn a_missing_source_is_an_error_not_an_empty_set() {
        let missing = tempfile::tempdir().unwrap().path().join("absent.rs");
        assert!(consumed_fields(&[missing]).is_err());
        assert!(decode_sources(Path::new("does-not-exist")).is_err());
    }

    /// Break caught: the source list stops matching the crate, so the scan runs
    /// against a subset and the unknown bucket fills with fields that are read.
    #[test]
    fn the_real_decode_sources_name_known_wire_fields() {
        let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let sources = decode_sources(crate_root).unwrap();
        assert!(sources.len() > 10, "{} sources", sources.len());
        assert!(
            sources.iter().all(|source| !source
                .components()
                .any(|part| part.as_os_str() == "capture")),
            "the recording rig is not a decode path"
        );

        let names = consumed_fields(&sources).unwrap();
        for known in ["tool_use_result", "session_id", "subtype"] {
            assert!(names.contains(known), "{known} missing from the scan");
        }
    }
}
