import { Container, getContainer } from "@cloudflare/containers";

/**
 * Wraps the memegen-rs Rust server (binds 0.0.0.0:5005 inside the container).
 * Pinned to a single instance (getContainer's built-in singleton id).
 */
export class MemegenContainer extends Container {
  defaultPort = 5005; // matches the Rust server's bind port
  sleepAfter = "10m"; // scale to zero when idle; pay only for active time
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
  async fetch(
    request: Request,
    env: Env,
    ctx: ExecutionContext,
  ): Promise<Response> {
    // The container is reached via the Durable Object binding, not a CDN
    // fetch(), so its responses are never auto-cached. We cache rendered images
    // explicitly: a HIT is served at the edge and never reaches the container,
    // so it costs nothing and doesn't count against the 5/s render limiter.
    if (request.method !== "GET") {
      return getContainer(env.MEMEGEN).fetch(request);
    }

    const url = new URL(request.url);
    const cache = caches.default;
    // Same URL == same image forever, so the URL alone is a perfect cache key.
    const key = new Request(url.toString(), { method: "GET" });

    const hit = await cache.match(key);
    if (hit) return hit;

    // Only a render that actually reaches the origin counts against the limit -
    // cache hits above are free. A single shared key caps aggregate render
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

    // Edge-cache only the content-derived assets (rendered images, thumbnails,
    // the font) - their URL fully determines their bytes. HTML and JSON are
    // skipped so they stay fresh on every deploy.
    const type = res.headers.get("content-type") ?? "";
    if (res.ok && (type.startsWith("image/") || type.startsWith("font/"))) {
      ctx.waitUntil(cache.put(key, res.clone()));
    }
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
