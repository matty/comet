# Syntax highlighting

Desktop syntax highlighting lives in the pure `comet-syntax` crate. It detects
languages, runs pinned Tree-sitter grammars and queries, and returns sorted,
non-overlapping UTF-8 byte spans relative to each source line. The UI resolves
those neutral `HighlightKind` values through `Theme::syntax`; parser code never
depends on GPUI or colors.

The bundled surface is Rust, JavaScript, JSX, TypeScript, TSX, Python, Go,
JSON, JSONC, Bash, TOML, Markdown, HTML, CSS, YAML, C, C++, C#, Java, Kotlin,
Swift, Ruby, PHP, SQL, Lua, Dockerfile/Containerfile, Nix, and Make. Detection
uses a fenced-code alias first, then the path or exact filename, then an
unambiguous shebang only when neither explicit hint resolved a language.

Markdown fences and tool diffs parse complete documents on GPUI's background
executor. Changes first parses separate old/new hunk excerpts, then lazily asks
the checkout host for checksum-bound complete sources. Deleted lines use the old
document; added and context lines prefer the new document. A stale checksum or
any visible-line mismatch discards the full result atomically.

Markdown and HTML injections resolve only bundled child grammars. Unknown
fences stay valid parent Markdown and their contents remain plain. The parser
accepts at most 1 MiB of source and 200,000 normalized spans per document and
supports cooperative cancellation.

The UI cache holds at most 96 neutral documents and 24 MiB of their materialized
line/span storage. Its SHA-256 key includes the language and query-generation
number, never theme colors, so appearance changes recolor cached spans without
reparsing. Checkout source enrichment is current-working-tree only: the owning
local or paired-LAN engine verifies checkout identity, snapshot checksum, diff
membership, bounded file reads, and a post-read checksum before returning a
source pair. The UI repeats checksum and visible-line validation before an
atomic promotion from excerpt to complete documents.

## Adding a grammar

1. Review the parser, generated sources, and queries' licenses. Pin an exact
   crate version in `crates/syntax/Cargo.toml` and add it to
   `THIRD_PARTY_NOTICES.md`.
2. Add aliases, extensions, exact filenames, and any unambiguous shebang to the
   central registry in `crates/syntax/src/lib.rs`.
3. Add its `HighlightConfiguration` using official compatible queries. Map new
   capture vocabulary to an existing `HighlightKind`; never expose capture names
   to the theme.
4. Add a minimal distinctive fixture to the query-load table. If the language
   supports injections, register only known child parsers and keep unknown
   injected languages plain.
5. Run `cargo test -p comet-syntax`, UI Markdown/Changes tests, the ignored
   diagnostic benchmark when parser cost changes, and the workspace checks.

Do not add language-specific parsing to a renderer. Unknown languages, binaries,
oversized sources, incompatible queries, and parse failures must remain plain.
Highlighting changes foreground color only—never font, weight, style, wrapping,
height, or scroll geometry.
