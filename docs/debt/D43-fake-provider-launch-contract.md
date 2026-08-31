# D43 — the fake providers do not prove the launch contract

The Claude and Codex integration suites cross a real process boundary, but the
child processes validate only part of what production must launch.

Claude's fake accepts every argument except an invalid `--permission-mode`.
Removing a required streaming flag, the stdio permission tool, `--model`, effort,
settings or `--resume` can therefore leave the subprocess suite green. No Claude
subprocess scenario sends an attachment, so the image-loading and first-message
path also stop at unit-shape coverage.

Codex checks the initialize payload and selected thread/turn parameters, but it
never checks that the executable was invoked with `app-server`. Its run scenarios
inspect the `cwd` field sent over JSON-RPC rather than the process's actual working
directory. That is weaker than command discovery's child-side cwd echo.

**Why this is debt.** These tests can prove that Comet understands a plausible
provider transcript while missing that the real CLI would never start, would
start in the wrong mode, or would receive a different first request.

Fix shape: make each fake validate the full invocation it depends on and report a
specific stderr failure when it differs. Add focused Claude resume and attachment
runs, a Codex `app-server` assertion, child-side cwd checks, and temporary paths
instead of `/tmp`. Keep the assertions provider-specific; a shared helper can own
argv lookup, cwd/environment capture and structured failure output.

D34 separately owns relative executable resolution and its regression coverage;
D43 should not absorb or block that fix.

## Resolution, 2026-08-31

The Claude and Codex integration suites now validate the invocation details
production depends on. Claude rejects a run that lacks its streaming/control argv
floor, while the existing launch-record scenario records the argv values and real
child cwd the OS actually delivered. Codex independently checks its `app-server`
subcommand and child-side cwd.

The final Claude gap was attachments. `an_attachment_reaches_the_child_in_the_first_message`
creates a temporary PNG, passes it as a real `RunRequest` attachment, and reads
the fake Claude child's launch record. It asserts the child's first stdin user
frame has the literal expected base64 `image/png` block before the original prompt
text. A skipped image load, wrong encoding, or reversed content order therefore
fails after crossing the subprocess boundary rather than only in wire-shape unit
coverage.

The record is deliberately the existing D43 launch-record mechanism, extended
with that first stdin frame, rather than a parallel attachment-specific channel.
D34 remains separate.
