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
  async fetch(request: Request, env: Env): Promise<Response> {
    const container = getContainer(env.MEMEGEN);
    return container.fetch(request);
  },
} satisfies ExportedHandler<Env>;
