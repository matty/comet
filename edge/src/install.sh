#!/bin/sh
# Comet (native) headless installer.
#
#   curl -fsSL https://comet.zeron.sh/install.sh | sh
#
# Uses Python 3 only while installing to strictly validate signed release
# metadata. Installs the self-contained native binary (no runtime deps) to
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
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
manifest_file="$tmp/manifest.json"
parsed_file="$tmp/manifest-fields"
curl -fsSL "$BASE/releases/manifest.json" -o "$manifest_file"

if ! command -v python3 >/dev/null 2>&1 || ! python3 -c 'import json' >/dev/null 2>&1; then
  echo "comet install: strict manifest validation requires python3" >&2
  exit 1
fi

python3 - "$manifest_file" "$plat" "$arch" > "$parsed_file" <<'PY'
import json
import re
import sys

def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result

def fail(message):
    print(f"comet install: {message}", file=sys.stderr)
    raise SystemExit(1)

try:
    with open(sys.argv[1], "r", encoding="utf-8") as source:
        manifest = json.load(source, object_pairs_hook=unique_object)
except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as error:
    fail(str(error))

if not isinstance(manifest, dict):
    fail("manifest JSON must be an object")
if manifest.get("repository") != "matty/comet":
    if "repository" not in manifest:
        fail("missing release repository (expected matty/comet)")
    fail(f"release repository mismatch (expected matty/comet, got {manifest.get('repository')!r})")
version = manifest.get("version")
if not isinstance(version, str):
    fail("invalid release version")
version = version.strip()
if not re.fullmatch(r"v*[0-9]+(?:\.[0-9]+)*", version):
    fail("invalid release version")
if any(int(part) > 18446744073709551615 for part in version.lstrip("v").split(".")):
    fail("invalid release version")
artifact = f"comet-{version}-{sys.argv[2]}-{sys.argv[3]}.tar.gz"
files = manifest.get("files")
metadata = files.get(artifact) if isinstance(files, dict) else None
if not isinstance(metadata, dict):
    fail(f"missing artifact metadata for {artifact}")
checksum = metadata.get("sha256")
if not isinstance(checksum, str) or not re.fullmatch(r"[0-9A-Fa-f]{64}", checksum):
    fail(f"invalid SHA-256 for {artifact}")
print(version)
print(artifact)
print(checksum.lower())
PY

ver="$(sed -n '1p' "$parsed_file")"
file="$(sed -n '2p' "$parsed_file")"
expected_sha256="$(sed -n '3p' "$parsed_file")"
expected_marker="repository=matty/comet
version=$ver
artifact=$file
sha256=$expected_sha256"
data_root="$HOME/.comet-native"
app_root="$data_root/app"
dest="$app_root/$ver"

if [ -f "$dest/comet" ]; then
  actual_marker=
  if [ -f "$dest/.comet-release" ]; then
    actual_marker="$(cat "$dest/.comet-release")"
  fi
  if [ "$actual_marker" != "$expected_marker" ]; then
    echo "comet install: unverified existing install at $dest; remove it before retrying" >&2
    exit 1
  fi
  echo "comet $ver already downloaded — relinking."
else
  echo "downloading comet $ver ($plat-$arch)…"
  curl -fSL --progress-bar "$BASE/releases/$file" -o "$tmp/$file"
  if command -v sha256sum >/dev/null 2>&1; then
    actual_sha256="$(sha256sum "$tmp/$file" | awk '{print tolower($1)}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual_sha256="$(shasum -a 256 "$tmp/$file" | awk '{print tolower($1)}')"
  else
    echo "comet install: SHA-256 verification requires sha256sum or shasum" >&2
    exit 1
  fi
  if [ "$actual_sha256" != "$expected_sha256" ]; then
    echo "comet install: checksum mismatch for $file" >&2
    exit 1
  fi
  mkdir -p "$dest"
  tar -xzf "$tmp/$file" -C "$dest" --strip-components=1
  [ -f "$dest/comet" ] || {
    echo "comet install: verified archive did not contain a comet binary" >&2
    rm -rf "$dest"
    exit 1
  }
  printf '%s\n' "$expected_marker" > "$dest/.comet-release"
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
