#!/bin/sh
# Comet (native) headless installer.
#
#   curl -fsSL https://comet.zeron.sh/install.sh | sh
#
# Installs the self-contained native binary (no runtime deps) to
# ~/.comet-native/app, puts `comet` on PATH, and runs it as a systemd user
# service that survives reboots. Re-running
# upgrades in place; ~/.comet-native state is preserved.
#
# Release distribution is separate from local/LAN operation. Override only the
# download source with COMET_RELEASES_URL when mirroring releases.
set -eu

BASE="${COMET_RELEASES_URL:-https://comet.zeron.sh}"

# --- platform ---------------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux) plat=linux ;;
  Darwin)
    echo "comet install: on macOS, download the desktop app instead:" >&2
    echo "  see $BASE/releases/manifest.json for the current macOS artifact" >&2
    exit 1
    ;;
  *)
    echo "comet install: unsupported OS '$os' — only Linux for now." >&2
    exit 1
    ;;
esac
case "$arch" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *)
    echo "comet install: unsupported architecture '$arch'." >&2
    exit 1
    ;;
esac

# --- download ----------------------------------------------------------------
manifest="$(curl -fsSL "$BASE/releases/manifest.json")"
compact_manifest="$(printf '%s' "$manifest" | tr -d '[:space:]')"
repository_fields="$(printf '%s' "$compact_manifest" | grep -o '"repository":"[^"]*"' || true)"
repository_count="$(printf '%s\n' "$repository_fields" | grep -c . || true)"
if [ "$repository_count" -eq 0 ]; then
  echo "comet install: missing release repository (expected matty/comet)" >&2
  exit 1
fi
if [ "$repository_count" -ne 1 ]; then
  echo "comet install: invalid release repository metadata" >&2
  exit 1
fi
repository="$(printf '%s' "$repository_fields" | sed -n 's/^"repository":"\([^"]*\)"$/\1/p')"
if [ "$repository" != "matty/comet" ]; then
  echo "comet install: release repository mismatch (expected matty/comet, got $repository)" >&2
  exit 1
fi
ver="$(printf '%s' "$compact_manifest" | sed -n 's/.*"version":"\([^"]*\)".*/\1/p')"
[ -n "$ver" ] || { echo "comet install: manifest has no release version" >&2; exit 1; }
file="comet-$ver-$plat-$arch.tar.gz"
data_root="$HOME/.comet-native"
app_root="$data_root/app"
dest="$app_root/$ver"

if [ -x "$dest/comet" ]; then
  echo "comet $ver already downloaded — relinking."
else
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  echo "downloading comet $ver ($plat-$arch)…"
  curl -fSL --progress-bar "$BASE/releases/$file" -o "$tmp/$file"
  mkdir -p "$dest"
  tar -xzf "$tmp/$file" -C "$dest" --strip-components=1
fi

ln -sfn "$dest" "$app_root/current"
mkdir -p "$HOME/.local/bin"
ln -sf "$app_root/current/comet" "$HOME/.local/bin/comet"

# --- service -----------------------------------------------------------------
service=manual
if command -v systemctl >/dev/null 2>&1 && [ -n "${XDG_RUNTIME_DIR:-}" ]; then
  mkdir -p "$HOME/.config/systemd/user"
  cat >"$HOME/.config/systemd/user/comet-native.service" <<'UNIT'
[Unit]
Description=Comet native headless engine
After=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
ExecStart=%h/.comet-native/app/current/comet headless
Restart=on-failure
RestartSec=5
EnvironmentFile=-%h/.comet-native/env

[Install]
WantedBy=default.target
UNIT
  systemctl --user daemon-reload
  systemctl --user enable comet-native >/dev/null 2>&1 || true
  systemctl --user restart comet-native
  service=running
  # Keep the user manager (and the engine) running without an active login.
  loginctl enable-linger "$USER" 2>/dev/null \
    || sudo -n loginctl enable-linger "$USER" 2>/dev/null \
    || echo "warn: could not enable linger — the engine stops when you log out (run: sudo loginctl enable-linger $USER)"
else
  echo "warn: systemd user session not available — run the engine manually with: comet headless"
fi

# --- agent CLIs ---------------------------------------------------------------
command -v claude >/dev/null 2>&1 || \
  echo "note: Claude Code CLI not found — install it with: curl -fsSL https://claude.ai/install.sh | bash"

case ":$PATH:" in
  *":$HOME/.local/bin:"*) path_hint="" ;;
  *) path_hint=' (add ~/.local/bin to your PATH)' ;;
esac

echo ""
echo "✓ comet $ver installed$path_hint"
echo ""
case "$service" in
  running)
    echo "the engine restarted with the new version."
    echo "  systemctl --user status comet-native    check the service"
    ;;
  manual)
    echo "next: run the engine with \`comet headless\`."
    ;;
esac
