/** Release distribution Worker. It is deliberately not a Comet runtime. */
import type { Env } from "./env";
import installSh from "./install.sh";

const notFound = (): Response => new Response("not found", { status: 404 });

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const readable = request.method === "GET" || request.method === "HEAD";
    if (!readable) return notFound();

    if (url.pathname === "/install.sh") {
      return new Response(request.method === "HEAD" ? null : installSh, {
        headers: {
          "content-type": "application/x-sh",
          "cache-control": "public, max-age=0, must-revalidate"
        }
      });
    }

    if (!url.pathname.startsWith("/releases/")) return notFound();
    let key: string;
    try {
      key = decodeURIComponent(url.pathname.slice("/releases/".length));
    } catch {
      return notFound();
    }
    if (!key || key.includes("..") || key.startsWith("/")) return notFound();

    const object = await env.RELEASES.get(key);
    if (!object) return notFound();
    const mutable = key.endsWith(".txt") || key.endsWith(".json");
    const headers = new Headers({
      "content-type": key.endsWith(".txt")
        ? "text/plain; charset=utf-8"
        : key.endsWith(".json")
          ? "application/json"
          : "application/octet-stream",
      "content-length": String(object.size),
      "cache-control": mutable ? "public, max-age=60" : "public, max-age=86400, immutable",
      etag: object.httpEtag
    });
    return new Response(request.method === "HEAD" ? null : object.body, { headers });
  }
} satisfies ExportedHandler<Env>;

export type { Env } from "./env";
