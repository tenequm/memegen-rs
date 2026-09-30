import { Container } from "@cloudflare/containers";

/**
 * The pre-Kubernetes origin: the Rust server in a Cloudflare Container. No
 * request reaches it any more (see `origin` below); the class stays exported
 * because the Durable Object binding still names it, and so that reverting the
 * origin swap is a one-commit rollback onto a container that is still deployed.
 */
export class MemegenContainer extends Container {
  defaultPort = 5005; // matches the Rust server's bind port
  sleepAfter = "10m"; // scale to zero when idle; pay only for active time
  // Brand watermark on rendered images. Unset locally (no watermark by
  // default); production bakes "memegen.rs" into every render.
  envVars = { MEMEGEN_WATERMARK: "memegen.rs" };
}

// `Env` (with MEMEGEN + RENDER_LIMITER) is generated from wrangler.jsonc
// bindings into worker-configuration.d.ts by `wrangler types`.

// Render endpoints are the only CPU-heavy paths (drawing, GIF encoding, the
// outbound fetch in /custom). The gallery, builder, docs, JSON, fonts, and
// on-disk thumbnails are cheap and stay unthrottled.
function isRenderPath(pathname: string): boolean {
  return pathname.startsWith("/images/");
}

// Renders happen on the Rust server in the Kubernetes cluster (ORIGIN in
// wrangler.jsonc). Path and query pass through unchanged; the server builds
// every URL relative, so nothing in a response names the origin host.
function origin(request: Request, env: Env): Promise<Response> {
  const url = new URL(request.url);
  return fetch(new Request(new URL(url.pathname + url.search, env.ORIGIN), request));
}

export default {
  async fetch(
    request: Request,
    env: Env,
    ctx: ExecutionContext,
  ): Promise<Response> {
    const url = new URL(request.url);

    // Secret-guarded purge hit by CI post-deploy: a deploy busts the
    // version-keyed cache, but the container rollout can repopulate it from the
    // old image, and images are `immutable`. Only the Worker can purge its own
    // Workers Cache (zone/dashboard purges can't).
    if (url.pathname === "/__purge") {
      const secret = (env as unknown as { PURGE_SECRET?: string }).PURGE_SECRET;
      if (!secret || request.headers.get("x-purge-secret") !== secret) {
        return new Response("forbidden\n", { status: 403 });
      }
      const cache = (
        ctx as unknown as {
          cache?: { purge(o: { purgeEverything: boolean }): Promise<void> };
        }
      ).cache;
      if (!cache) return new Response("purge unavailable\n", { status: 501 });
      await cache.purge({ purgeEverything: true });
      return new Response("purged\n", { status: 200 });
    }

    // Caching lives in Workers Caching (`cache.enabled` in wrangler.jsonc), not
    // here: it is tiered (one render anywhere fills a network-wide upper tier,
    // unlike the per-datacenter Cache API) and collapses concurrent requests
    // for the same URL into a single origin call. Cache HITs never invoke this Worker at all, so
    // everything below runs only on a true miss. Lifetimes come from the
    // Cache-Control/CDN-Cache-Control headers the Rust server sets; the cache
    // key includes the Worker version, so every deploy busts it.
    if (request.method !== "GET") {
      return origin(request, env);
    }

    // Only a render that actually reaches the origin counts against the limit -
    // cache hits never get here. A single shared key caps aggregate render
    // throughput per location (a bill backstop, not a per-user limit), so a
    // distributed flood can't multiply the cost across many IPs.
    if (isRenderPath(url.pathname)) {
      const { success } = await env.RENDER_LIMITER.limit({ key: "render" });
      if (!success) {
        return new Response("429: render capacity is busy, try again shortly", {
          status: 429,
          headers: { "retry-after": "10", "content-type": "text/plain" },
        });
      }
    }

    const res = await origin(request, env);

    const type = res.headers.get("content-type") ?? "";
    if (env.EXTRA_HTML_SCRIPTS && type.startsWith("text/html")) {
      return new HTMLRewriter()
        .on("head", {
          element(el) {
            el.append(env.EXTRA_HTML_SCRIPTS, { html: true });
          },
        })
        .transform(res);
    }
    return res;
  },
} satisfies ExportedHandler<Env>;
