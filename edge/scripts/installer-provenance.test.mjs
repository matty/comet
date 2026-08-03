import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const installer = fileURLToPath(new URL("../src/install.sh", import.meta.url));
const bash = process.platform === "win32" ? "C:\\Program Files\\Git\\bin\\bash.exe" : "bash";
const harness = String.raw`
uname() { [ "$1" = "-s" ] && printf Linux || printf x86_64; }
curl() {
  case "$*" in
    *manifest.json*) printf '%s' "$TEST_MANIFEST" ;;
    *) printf 'unexpected download' ;;
  esac
}
export -f uname curl
. "$INSTALLER"
`;

const rejects = (manifest, expected) => {
  const run = spawnSync(bash, ["-c", harness], {
    encoding: "utf8",
    env: { ...process.env, INSTALLER: installer, TEST_MANIFEST: manifest }
  });
  const output = `${run.stdout}\n${run.stderr}`;
  if (run.status === 0 || !output.includes(expected)) {
    throw new Error(`installer did not reject manifest as ${expected}: status=${run.status}\n${output}`);
  }
};

const acceptsProvenance = (manifest) => {
  const run = spawnSync(bash, ["-c", harness], {
    encoding: "utf8",
    env: { ...process.env, INSTALLER: installer, TEST_MANIFEST: manifest }
  });
  const output = `${run.stdout}\n${run.stderr}`;
  if (!output.includes("downloading comet 1.2.3") || output.includes("release repository")) {
    throw new Error(`installer did not accept canonical provenance: status=${run.status}\n${output}`);
  }
};

rejects('{"version":"1.2.3"}', "missing release repository");
rejects(
  '{"repository":"someone/other-comet","version":"1.2.3"}',
  "release repository mismatch"
);
acceptsProvenance('{"repository":"matty/comet","version":"1.2.3"}');
