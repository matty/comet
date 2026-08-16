//! SNAPSHOT — the capability sheet (design §3.5).
//!
//! `render_sheet` is a pure function: evidence in, one markdown document out.
//! No filesystem, no `std::env`, no opinion about where the bytes end up —
//! the golden test in `capture_corpus/capability_sheets.rs` walks the
//! archive with [`super::surface::observe_surface`], reads each scenario's
//! `manifest.json` for its argv, and hands the result here. This module
//! never touches either.
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
/// after Task 3's review checked the manifests) is what makes three
/// genuinely identical scenarios — Claude's `model-discovery`,
/// `model-discovery-neutral-cwd` and `model-discovery-project-cwd` — read as
/// what they are: the same program, the same args
/// (`--print --input-format stream-json --output-format stream-json
/// --verbose --bare`), and the same redacted `cwd` (`<CWD>`). **Nothing in
/// the archive distinguishes them** — the working directory is the one real
/// difference between the three runs, and it is redacted to a placeholder
/// and unrecoverable from what's committed. Printing the argv still
/// discharges D80, but for the opposite reason an earlier draft of this
/// comment gave: three identical fenced blocks read as one experiment
/// printed three times, not as three independent confirmations — which is
/// exactly the impression a reader should take away.
#[derive(Clone, Debug)]
pub struct SheetScenario {
    pub name: String,
    pub purpose: String,
    /// `command.program` followed by `command.args`, verbatim from the
    /// manifest — including whatever redaction placeholder the archive
    /// already put there (`<CWD>`, `<HOME>`, `<REDACTED_1>`, …). Rendering
    /// starts at `argv[0]`; there is no separate program field.
    pub argv: Vec<String>,
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
         = \"1\"; cargo test -p comet-harness --test capture_corpus`; do not hand-edit."
            .to_owned(),
        String::new(),
        "This file reports only what the scenarios below actually produced. Diffing this \
         sheet against another version's sheet is the version-change report (no differ is \
         planned) — but before reading a disappearance as the CLI dropping a capability, \
         check the Scenarios section of both sheets. A field or a vocabulary value present \
         in one version and absent in the other may mean that version's captures simply \
         never exercised it, not that the CLI changed; the corpus's blind spot is absence, \
         and it did not go away just because this sheet makes it visible."
            .to_owned(),
        String::new(),
        "Two readings that argv makes tempting and both wrong: identical launch flags do \
         not mean identical coverage — a frame or reply that depends on something actually \
         happening during the run only appears when a run produced that trigger, so the \
         same flag present in both versions' scenarios is not evidence the underlying \
         event fired in both. And a field or value that is new in one version is not \
         necessarily a new capability — it can be account or environment state that simply \
         did not happen to occur during the other version's runs, not a wire-format \
         change. Argv and scenario names narrow what to check; they do not settle it on \
         their own."
            .to_owned(),
        String::new(),
    ]
}

fn scenario_lines(scenarios: &[SheetScenario]) -> Vec<String> {
    let mut lines = vec![
        "## Scenarios".to_owned(),
        String::new(),
        "Every scenario this sheet's evidence is drawn from, with the exact argv Comet \
         launched it with (redaction placeholders are the archive's, not this sheet's). A \
         capability no scenario here exercises cannot appear in the sections below, \
         whatever the wire format might otherwise support — this list is what makes that \
         limit visible instead of silent. A distinct name is not proof of distinct \
         coverage, either: two scenarios below with the same argv were launched \
         identically whatever their purpose sentences say, and two with the same purpose \
         sentence can still differ in argv — compare the argv itself before concluding two \
         scenarios tested different things, rather than trusting the name or the purpose \
         alone."
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
        lines.push("```".to_owned());
        for arg in &scenario.argv {
            lines.push(arg.clone());
        }
        lines.push("```".to_owned());
        lines.push(String::new());
    }
    lines
}

fn field_lines(provider: &str, version: &str, observations: &[FieldObservation]) -> Vec<String> {
    let mut lines = vec![
        "## Fields".to_owned(),
        String::new(),
        "Every dotted path observed on the wire for this provider and version, split by the \
         direction it travelled — `To provider` is what Comet sends, `From provider` is \
         what the provider sends back — one path per line, sorted. Read an absent path \
         against the Scenarios section above before reading it as a claim about the wire \
         format."
            .to_owned(),
        String::new(),
    ];

    for (heading, direction) in [
        ("To provider", Direction::ToProvider),
        ("From provider", Direction::FromProvider),
    ] {
        lines.push(format!("### {heading}"));
        lines.push(String::new());

        let paths: BTreeSet<&str> = observations
            .iter()
            .filter(|observation| {
                observation.provider == provider
                    && observation.version == version
                    && observation.direction == direction
            })
            .map(|observation| observation.path.as_str())
            .collect();

        if paths.is_empty() {
            lines.push("(none observed)".to_owned());
        } else {
            for path in paths {
                lines.push(format!("- `{path}`"));
            }
        }
        lines.push(String::new());
    }
    lines
}

fn vocabulary_lines(vocabulary: &BTreeMap<(Direction, String), BTreeSet<String>>) -> Vec<String> {
    let mut lines = vec![
        "## Vocabulary".to_owned(),
        String::new(),
        "The observed value set for a small declared list of discriminator paths — not \
         every field, only the ones whose values name what kind of thing a frame or a tool \
         call is (`VOCABULARY_PATHS` in `crates/harness/src/capture/surface.rs`). Every \
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
        FieldObservation {
            provider: provider.to_owned(),
            version: version.to_owned(),
            path: path.to_owned(),
            direction,
            first_seen: FrameRef {
                scenario: format!("{provider}/{version}/test"),
                sequence: 1,
            },
        }
    }

    fn scenario(name: &str, purpose: &str, argv: &[&str]) -> SheetScenario {
        SheetScenario {
            name: name.to_owned(),
            purpose: purpose.to_owned(),
            argv: argv.iter().map(|arg| arg.to_string()).collect(),
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
        // `"initialize"` only works as a Claude-only literal against THIS
        // synthetic evidence, which deliberately avoids it. Codex's own real
        // corpus is not Claude-only here: `.method` is `"initialize"` for
        // Comet's own request on `codex/0.147.0/model-discovery`'s first
        // frame (`stdin`, to-provider), so the real `codex-0.147.0.md`
        // legitimately contains this exact string in its Vocabulary section.
        // Do not repoint this test's `observations`/`vocabulary` at the real
        // corpus without first dropping `"initialize"` from this list — doing
        // so would fail for a correct reason (real Codex evidence) rather
        // than the leak this test exists to catch.
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
