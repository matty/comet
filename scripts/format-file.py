#!/usr/bin/env python3
"""Format a single edited file, the way this repo formats it.

Agent-agnostic entry point. Two calling conventions:

    python scripts/format-file.py crates/ui/src/theme.rs   # explicit path
    ... | python scripts/format-file.py                    # hook JSON on stdin

The second form is what Claude Code's PostToolUse hook uses: it reads the tool
payload and pulls out ``tool_response.filePath`` or ``tool_input.file_path``.
Python rather than bash + jq so this works on Windows, macOS, and Linux with
only the interpreter this repo already requires.

Unknown extensions, missing formatters, and formatter errors are a silent no-op
and always exit 0 — this runs after every edit and must never be the thing that
fails an agent's turn.
"""

import json
import shutil
import subprocess
import sys
from pathlib import Path


def target_from_stdin() -> str | None:
    if sys.stdin is None or sys.stdin.isatty():
        return None
    try:
        payload = json.loads(sys.stdin.read() or "{}")
    except (json.JSONDecodeError, OSError):
        return None
    if not isinstance(payload, dict):
        return None
    response = payload.get("tool_response")
    if isinstance(response, dict) and isinstance(response.get("filePath"), str):
        return response["filePath"]
    tool_input = payload.get("tool_input")
    if isinstance(tool_input, dict) and isinstance(tool_input.get("file_path"), str):
        return tool_input["file_path"]
    return None


def run(command: list[str]) -> None:
    try:
        subprocess.run(
            command,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
    except OSError:
        pass


def format_file(path: Path) -> None:
    suffix = path.suffix.lower()
    if suffix == ".rs":
        rustfmt = shutil.which("rustfmt")
        if rustfmt:
            # The edition must be explicit: rustfmt on a bare path does not read
            # the workspace Cargo.toml. It also formats the modules the file
            # declares (stable rustfmt has no --skip-children), which matches
            # what `cargo fmt --all` would do anyway.
            run([rustfmt, "--edition", "2024", str(path)])
        return
    if suffix == ".py":
        ruff = shutil.which("ruff")
        if ruff:
            run([ruff, "format", str(path)])
            return
        black = shutil.which("black")
        if black:
            run([black, "-q", str(path)])


def main() -> int:
    raw = sys.argv[1] if len(sys.argv) > 1 else target_from_stdin()
    if not raw:
        return 0
    path = Path(raw)
    if path.is_file():
        format_file(path)
    return 0


if __name__ == "__main__":
    sys.exit(main())
