import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../..", import.meta.url));
const config = JSON.parse(readFileSync(`${root}/edge/wrangler.jsonc`, "utf8"));
const workflow = readFileSync(`${root}/.github/workflows/deploy.yml`, "utf8");
const release = readFileSync(`${root}/.github/workflows/release.yml`, "utf8");

const migration = config.migrations?.at(-1);
const migrationTags = config.migrations?.map((entry) => entry.tag) ?? [];
if (
  new Set(migrationTags).size !== migrationTags.length ||
  config.migrations?.[0]?.tag !== "v1" ||
  !config.migrations[0].new_sqlite_classes?.includes("SessionRoom") ||
  !config.migrations[0].new_sqlite_classes?.includes("DeviceRoom") ||
  migration?.tag !== "v2-distribution-only" ||
  JSON.stringify([...migration.deleted_classes].sort()) !== JSON.stringify(["DeviceRoom", "SessionRoom"])
) {
  throw new Error("wrangler migration history does not uniquely delete both legacy Durable Objects");
}
if (config.durable_objects || config.vars || config.r2_buckets?.some((b) => b.binding !== "RELEASES")) {
  throw new Error("distribution Worker retains an obsolete runtime binding or variable");
}
const routes = config.routes?.map((route) => route.pattern).sort();
if (
  JSON.stringify(routes) !==
  JSON.stringify(["comet.zeron.sh/install.sh", "comet.zeron.sh/releases/*"].sort())
) {
  throw new Error("distribution Worker retains an obsolete route");
}
if (!workflow.includes("wrangler secret delete WORKOS_API_KEY") || !workflow.includes("continue-on-error: true")) {
  throw new Error("deployment does not idempotently clean up obsolete WORKOS_API_KEY");
}
if (!release.includes("github.repository == 'matty/comet'") || !release.includes("--arg repository \"matty/comet\"")) {
  throw new Error("release publishing is not pinned to the canonical repository");
}
if (release.includes("latest.txt")) {
  throw new Error("release workflow still publishes unprovenanced latest.txt metadata");
}
