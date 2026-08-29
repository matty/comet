#!/usr/bin/env python3
"""Probe an ACP agent's real JSON-RPC surface for its model/effort setters.

This is the script that produced the transcript behind PR7's central
finding (`crates/harness/src/acp/grok.rs`'s module doc): a first design sent
`session/set_config_option` for both Grok's model and effort selection,
inferred from the ACP org's own reference SDK schema. That inference was
wrong -- Grok answers `-32601 Method not found` for it. Only
`session/set_model` (the ACP spec's own dedicated method) turned out to be
real on grok 1.0.5, and no working effort setter was found among the
methods tried.

**Why a standalone script rather than a Rust test.** Every candidate here
is tried BEFORE `session/prompt` is ever sent, so the whole run costs zero
tokens against a real, rate-limited account -- which is exactly what let
this probe keep working through the free-quota exhaustion that blocked
PR7's own turn-content live checks (`acp_run_fidelity_grok_live.rs`). A
Rust integration test would need its own harness plumbing for the same
zero-cost property; a raw script hitting the wire directly is simpler and
faster to iterate on while hunting for a method name, and this one is kept
around specifically so THIS probe does not have to be re-invented the next
time a CLI update needs re-checking.

Usage (Windows / PowerShell, matching this repo's `GROK_EXECUTABLE`
convention -- pass the exe explicitly since PATH resolution timing is a
known trap here, see `crates/harness/src/acp/grok.rs`):

    python scripts/probe-acp-setters.py --exe "C:/Users/<you>/.grok/bin/grok.exe" --args --no-auto-update agent --no-leader stdio

For Hermes (or any other ACP agent), pass its own launch line the same way:

    python scripts/probe-acp-setters.py --exe hermes --args acp

Sends `initialize` and `session/new` (both token-free on every agent this
was tried against), then tries each candidate `(method, params)` pair in
`CANDIDATES` below and prints the raw reply -- extend that list to probe a
setter this file does not already know about. Never sends `session/prompt`,
so it never spends tokens on its own.
"""

from __future__ import annotations

import argparse
import json
import queue
import subprocess
import tempfile
import threading
import time


def spawn(exe: str, args: list[str]) -> subprocess.Popen:
    return subprocess.Popen(
        [exe, *args],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        bufsize=1,
    )


class Client:
    """A minimal newline-framed JSON-RPC client over one child's stdio."""

    def __init__(self, proc: subprocess.Popen) -> None:
        self.proc = proc
        self._next_id = 0
        self._queue: "queue.Queue[dict]" = queue.Queue()
        threading.Thread(target=self._read_loop, daemon=True).start()

    def _read_loop(self) -> None:
        assert self.proc.stdout is not None
        for line in self.proc.stdout:
            try:
                self._queue.put(json.loads(line))
            except (json.JSONDecodeError, UnicodeDecodeError):
                continue

    def next_id(self) -> int:
        self._next_id += 1
        return self._next_id

    def send(self, obj: dict) -> None:
        assert self.proc.stdin is not None
        self.proc.stdin.write((json.dumps(obj) + "\n").encode())
        self.proc.stdin.flush()

    def request(self, method: str, params: dict, timeout: float = 15.0) -> dict | None:
        """Send one request and return its reply, dropping notifications."""
        req_id = self.next_id()
        self.send({"jsonrpc": "2.0", "id": req_id, "method": method, "params": params})
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                frame = self._queue.get(timeout=0.3)
            except queue.Empty:
                continue
            if frame.get("id") == req_id and "method" not in frame:
                return frame
        return None


# Extend this list to probe a setter this file does not already know about.
# Each entry is (method, params-minus-sessionId); `sessionId` is filled in
# once the real session id is known.
CANDIDATES: list[tuple[str, dict]] = [
    ("session/set_model", {"modelId": "REPLACE_WITH_A_REAL_MODEL_ID"}),
    ("session/set_config_option", {"configId": "model", "value": "REPLACE_WITH_A_REAL_MODEL_ID"}),
    ("session/set_mode", {"modeId": "low"}),
    ("session/set_mode", {"modeId": "not-a-real-mode-xyz"}),
]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--exe", required=True, help="path to (or name of) the ACP agent's executable")
    parser.add_argument(
        "--args",
        nargs=argparse.REMAINDER,
        default=[],
        help="everything after this flag is passed through as launch args, "
        "e.g. --args --no-auto-update agent --no-leader stdio",
    )
    parser.add_argument("--cwd", default=None, help="cwd to open the session in (defaults to a temp dir)")
    args = parser.parse_args()

    cwd = args.cwd or tempfile.gettempdir()
    proc = spawn(args.exe, args.args)
    client = Client(proc)

    init_reply = client.request(
        "initialize",
        {
            "protocolVersion": 1,
            "clientInfo": {"name": "comet-native", "title": "Comet", "version": "probe"},
            "clientCapabilities": {"fs": {"readTextFile": False, "writeTextFile": False}, "terminal": False},
        },
    )
    print("initialize:", json.dumps(init_reply, indent=2)[:2000])

    new_reply = client.request("session/new", {"cwd": cwd, "mcpServers": []})
    print("session/new:", json.dumps(new_reply, indent=2)[:4000])
    if not new_reply or "result" not in new_reply:
        print("session/new did not answer with a result; stopping before any candidate probes")
        proc.terminate()
        return
    session_id = new_reply["result"]["sessionId"]
    print("session_id:", session_id)

    for method, params in CANDIDATES:
        full_params = {"sessionId": session_id, **params}
        reply = client.request(method, full_params)
        print(f"{method} {full_params}: {json.dumps(reply)}")

    proc.terminate()


if __name__ == "__main__":
    main()
