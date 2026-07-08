import { Container, getContainer } from "@cloudflare/containers";

/**
 * Wraps the memegen-rs Rust server (binds 0.0.0.0:5005 inside the container).
 * Pinned to a single instance (getContainer's built-in singleton id).
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

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    // Caching lives in Workers Caching (`cache.enabled` in wrangler.jsonc), not
    // here: it is tiered (one render anywhere fills a network-wide upper tier,
    // unlike the per-datacenter Cache API) and collapses concurrent requests
    // for the same URL into a single container call - both matter during the
    // scale-to-zero cold start. Cache HITs never invoke this Worker at all, so
    // everything below runs only on a true miss. Lifetimes come from the
    // Cache-Control/CDN-Cache-Control headers the Rust server sets; the cache
    // key includes the Worker version, so every deploy busts it.
    if (request.method !== "GET") {
      return getContainer(env.MEMEGEN).fetch(request);
    }

    const url = new URL(request.url);

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

    const res = await getContainer(env.MEMEGEN).fetch(request);

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
