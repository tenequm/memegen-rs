import { Container, getContainer } from "@cloudflare/containers";

/**
 * Wraps the memegen-rs Rust server (binds 0.0.0.0:5005 inside the container).
 * Pinned to a single instance so the in-container 5 req/s limiter is a true
 * global cap (a global limit and multi-instance fan-out are mutually exclusive).
 */
export class MemegenContainer extends Container {
  defaultPort = 5005; // matches the Rust server's bind port
  sleepAfter = "10m"; // scale to zero when idle; pay only for active time
}

interface Env {
  MEMEGEN: DurableObjectNamespace<MemegenContainer>;
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

    const cache = caches.default;
    // Same URL == same image forever, so the URL alone is a perfect cache key.
    const key = new Request(new URL(request.url).toString(), { method: "GET" });

    const hit = await cache.match(key);
    if (hit) return hit;

    const res = await getContainer(env.MEMEGEN).fetch(request);

    // Cache only immutable assets - the Rust origin marks rendered images,
    // thumbnails, and the font with a long `immutable` Cache-Control. HTML and
    // JSON carry none, so they stay fresh on every deploy.
    const type = res.headers.get("content-type") ?? "";
    if (res.ok && (type.startsWith("image/") || type.startsWith("font/"))) {
      ctx.waitUntil(cache.put(key, res.clone()));
    }
    return res;
  },
} satisfies ExportedHandler<Env>;
