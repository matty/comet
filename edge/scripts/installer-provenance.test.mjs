import { createHash } from "node:crypto";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const installer = fileURLToPath(new URL("../src/install.sh", import.meta.url));
const bash = process.platform === "win32" ? "C:\\Program Files\\Git\\bin\\bash.exe" : "bash";
const artifact = "canonical artifact bytes";
const checksum = createHash("sha256").update(artifact).digest("hex");
const pythonShim =
  process.platform === "win32"
    ? 'python3() { command python "$@"; }\nexport -f python3\n'
    : "";
const harness = String.raw`
${pythonShim}
if [ "$TEST_HIDE_PYTHON" = 1 ]; then
  python3() { return 127; }
  export -f python3
fi
uname() { [ "$1" = "-s" ] && printf Linux || printf x86_64; }
curl() {
  output=
  url=
  while [ "$#" -gt 0 ]; do
    case "$1" in
      -o) output="$2"; shift 2 ;;
      http*) url="$1"; shift ;;
      *) shift ;;
    esac
  done
  case "$url" in
    */manifest.json) payload="$TEST_MANIFEST" ;;
    *) payload="$TEST_ARTIFACT" ;;
  esac
  if [ -n "$output" ]; then printf '%s' "$payload" > "$output"; else printf '%s' "$payload"; fi
}
tar() {
  destination=
  while [ "$#" -gt 0 ]; do
    if [ "$1" = -C ]; then destination="$2"; shift 2; else shift; fi
  done
  mkdir -p "$destination"
  printf executable > "$destination/comet"
  chmod +x "$destination/comet"
  : > "$TEST_TAR_MARKER"
}
export -f uname curl tar
. "$INSTALLER"
`;

const shellPath = (path) =>
  process.platform === "win32"
    ? path.replaceAll("\\", "/").replace(/^([A-Za-z]):/, (_, drive) => `/${drive.toLowerCase()}`)
    : path;

const runInstaller = (manifest, options = {}) => {
  const home = mkdtempSync(join(tmpdir(), "comet-installer-provenance-"));
  const marker = join(home, "tar-called");
  if (options.existingInstall) {
    const destination = join(home, ".comet-native", "app", "1.2.3");
    mkdirSync(destination, { recursive: true });
    const executable = join(destination, "comet");
    writeFileSync(executable, "existing");
    chmodSync(executable, 0o755);
    if (options.stageMarker !== undefined) {
      writeFileSync(join(destination, ".comet-release"), options.stageMarker);
    }
  }
  const run = spawnSync(bash, ["-c", harness], {
    encoding: "utf8",
    env: {
      ...process.env,
      HOME: shellPath(home),
      INSTALLER: installer,
      TEST_ARTIFACT: artifact,
      TEST_MANIFEST: manifest,
      TEST_HIDE_PYTHON: options.hidePython ? "1" : "0",
      TEST_TAR_MARKER: shellPath(marker)
    }
  });
  const installedMarker = join(home, ".comet-native", "app", "1.2.3", ".comet-release");
  const result = {
    run,
    output: `${run.stdout}\n${run.stderr}`,
    unpacked: existsSync(marker),
    installedMarker: existsSync(installedMarker) ? readFileSync(installedMarker, "utf8") : undefined
  };
  rmSync(home, { recursive: true, force: true });
  return result;
};

const rejects = (manifest, expected, options = {}) => {
  const result = runInstaller(manifest, options);
  if (result.run.status === 0 || !result.output.includes(expected) || result.unpacked) {
    throw new Error(
      `installer did not fail closed as ${expected}: status=${result.run.status}, unpacked=${result.unpacked}\n${result.output}`
    );
  }
};

const expectedStageMarker = `repository=matty/comet\nversion=1.2.3\nartifact=comet-1.2.3-linux-x86_64.tar.gz\nsha256=${checksum}\n`;

const accepts = (manifest) => {
  const result = runInstaller(manifest);
  if (
    !result.unpacked ||
    result.installedMarker !== expectedStageMarker ||
    result.output.includes("release repository") ||
    result.output.includes("checksum mismatch")
  ) {
    throw new Error(
      `installer rejected valid signed metadata: status=${result.run.status}, unpacked=${result.unpacked}\n${result.output}`
    );
  }
};

rejects('{"version":"1.2.3"}', "missing release repository");
rejects('{"repository":"someone/other-comet","version":"1.2.3"}', "release repository mismatch");
rejects(
  '{"repository":"matty/comet","repository":"someone/other-comet","version":"1.2.3"}',
  "duplicate JSON key"
);
rejects('{"repository":"matty/comet","version":"../../evil"}', "invalid release version");
rejects('{"repository":"matty/comet","version":"1.2 3"}', "invalid release version");
rejects(
  '{"repository":"matty/comet","version":"18446744073709551616.1"}',
  "invalid release version"
);
rejects('{"repository":"matty/comet","version":"1.2.3","files":{}}', "missing artifact metadata");
rejects(
  '{"repository":"matty/comet","version":"1.2.3","files":{"comet-1.2.3-linux-x86_64.tar.gz":{"sha256":"bad"}}}',
  "invalid SHA-256"
);
rejects(
  `{"repository":"matty/comet","version":"1.2.3","files":{"comet-1.2.3-linux-x86_64.tar.gz":{"sha256":"${"0".repeat(64)}"}}}`,
  "checksum mismatch"
);
accepts(
  `{"repository":"matty\\u002fcomet","version":"1.2.3","files":{"comet-1.2.3-linux-x86_64.tar.gz":{"sha256":"${checksum}"}}}`
);

const validManifest = `{"repository":"matty/comet","version":"1.2.3","files":{"comet-1.2.3-linux-x86_64.tar.gz":{"sha256":"${checksum}"}}}`;
rejects(validManifest, "strict manifest validation requires python3", { hidePython: true });

for (const stageMarker of [undefined, "repository=matty/comet\nsha256=wrong\n"]) {
  const result = runInstaller(validManifest, { existingInstall: true, stageMarker });
  if (result.run.status === 0 || !result.output.includes("unverified existing install") || result.unpacked) {
    throw new Error(`installer trusted an unverified existing install\n${result.output}`);
  }
}

const verifiedReuse = runInstaller(validManifest, {
  existingInstall: true,
  stageMarker: expectedStageMarker
});
if (
  verifiedReuse.run.status !== 0 ||
  verifiedReuse.unpacked ||
  !verifiedReuse.output.includes("already downloaded — relinking")
) {
  throw new Error(`installer did not securely reuse a verified install\n${verifiedReuse.output}`);
}
