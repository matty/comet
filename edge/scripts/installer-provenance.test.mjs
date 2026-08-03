import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
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
tar() { : > "$TEST_TAR_MARKER"; }
export -f uname curl tar
. "$INSTALLER"
`;

const runInstaller = (manifest) => {
  const home = mkdtempSync(join(tmpdir(), "comet-installer-provenance-"));
  const marker = join(home, "tar-called");
  const run = spawnSync(bash, ["-c", harness], {
    encoding: "utf8",
    env: {
      ...process.env,
      HOME: home,
      INSTALLER: installer,
      TEST_ARTIFACT: artifact,
      TEST_MANIFEST: manifest,
      TEST_TAR_MARKER: marker
    }
  });
  const result = { run, output: `${run.stdout}\n${run.stderr}`, unpacked: existsSync(marker) };
  rmSync(home, { recursive: true, force: true });
  return result;
};

const rejects = (manifest, expected) => {
  const result = runInstaller(manifest);
  if (result.run.status === 0 || !result.output.includes(expected) || result.unpacked) {
    throw new Error(
      `installer did not fail closed as ${expected}: status=${result.run.status}, unpacked=${result.unpacked}\n${result.output}`
    );
  }
};

const accepts = (manifest) => {
  const result = runInstaller(manifest);
  if (!result.unpacked || result.output.includes("release repository") || result.output.includes("checksum mismatch")) {
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
