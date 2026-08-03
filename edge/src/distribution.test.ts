import { describe, expect, it, vi } from "vitest";

vi.mock("./install.sh", () => ({ default: "#!/bin/sh\necho install\n" }));

import worker, { type Env } from "./index";

const body = new TextEncoder().encode('{"version":"1.2.3"}');
const releaseObject = {
  body,
  size: body.byteLength,
  httpEtag: '"manifest"'
};

const env = {
  RELEASES: {
    get: async (key: string) => (key === "manifest.json" ? releaseObject : null)
  } as unknown as R2Bucket
} satisfies Env;

const request = (path: string, method = "GET") =>
  worker.fetch(new Request(`https://downloads.example${path}`, { method }), env);

describe("distribution-only worker", () => {
  it.each([
    "/health",
    "/auth/exchange",
    "/workspace/example/ws",
    "/device/example/ws",
    `/attachments/${"a".repeat(64)}`
  ])("does not route removed runtime endpoint %s", async (path) => {
    expect((await request(path)).status).toBe(404);
  });

  it("serves only GET and HEAD for the installer", async () => {
    expect((await request("/install.sh")).status).toBe(200);
    expect((await request("/install.sh", "HEAD")).status).toBe(200);
    expect((await request("/install.sh", "POST")).status).toBe(404);
  });

  it("serves only GET and HEAD from the release bucket", async () => {
    expect((await request("/releases/manifest.json")).status).toBe(200);
    expect((await request("/releases/manifest.json", "HEAD")).status).toBe(200);
    expect((await request("/releases/manifest.json", "PUT")).status).toBe(404);
  });
});
