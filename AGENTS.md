# AGENTS.md - memegen-rs

Stateless meme-generator HTTP API + web UI in **pure Rust**. Every meme is fully described by its URL: no DB, no cache server, no login. A minimal Rust reimplementation of [jacebrowning/memegen](https://github.com/jacebrowning/memegen). Production: <https://memegen.rs>.

## Layout

- `src/template.rs` - model, in-memory registry (read once at startup), URL codec, styling.
- `src/render.rs` - rendering pipeline (autosize, wrap, outline, composite, GIF encode).
- `src/main.rs` - axum router, handlers, OpenAPI, error mapping, web UI (maud, compile-time).
- `ops/worker/` - Cloudflare Worker edge layer (`worker.ts`: cache + rate limit + analytics injection) and its toolchain (`wrangler.jsonc`, `package.json`, `tsconfig.json`, generated `worker-configuration.d.ts`). Not Rust.
- `ops/docker/` - `Containerfile` + `Containerfile.dockerignore` for the container image build.
- `templates/<id>/` - 701 template folders, each `config.yml` (upstream memegen schema) + `default.{png,jpg,webp,gif}`. `templates/popularity.json` ranks them. **Committed to the repo and baked into the image.**
- `assets/` - embedded fonts (Anton, Pangolin; SIL OFL), favicons, OG image, `SKILL.md` (the ClawHub agent skill; `SKILL.md` at root is a symlink to it). `Anton-Regular.ttf` is the Cyrillic-extended v2.300 build from [Tural/AntonFont](https://github.com/Tural/AntonFont) (unmerged upstream as [google/fonts#7552](https://github.com/google/fonts/issues/7552)); both fonts cover Latin + full Cyrillic/Ukrainian.

## Stack

axum (HTTP) + utoipa/Scalar (`/docs`, `/openapi.json`) + maud (compile-time UI) + image/imageproc/ab_glyph (render) + serde-saphyr (pure-Rust YAML) + fast_image_resize (SIMD thumbnails) + reqwest/rustls (`?background=` fetch). Fonts embedded via `include_bytes!`. Edition 2024, Rust 1.95 (pinned in `rust-toolchain.toml`).

## Dev loop

```sh
cargo run                 # local server on :5005, reads ./templates (override: MEMEGEN_TEMPLATES_DIR)
cargo fmt && cargo clippy && cargo test   # validate; run before every commit
```

Lints are strict (`unsafe_code = forbid`, clippy `all = deny`) - CI fails on warnings. `cargo clippy` does **not** emit a binary; rebuild with `cargo run`/`cargo build` before smoke-testing or you'll hit a stale executable.

Optional: `cd ops/worker && npm run dev` (`wrangler dev`) runs the Worker edge layer locally; not needed for pure Rust/render work.

### Smoke test (against a running `:5005`)

```sh
curl -sf 'http://localhost:5005/images/drake/writing_a_parser/just_using_a_url.png' -o /tmp/m.png  # render
curl -sf 'http://localhost:5005/templates' | head                                                  # registry JSON
curl -sf 'http://localhost:5005/' -o /dev/null && echo ok                                           # web UI / docs
```

Prod smoke test: same paths against `https://memegen.rs` (expect cold-start latency on first render).

## Releasing a new version

Deploy and release are **separate** pipelines:

```sh
git push origin main      # -> deploy.yml auto-deploys to prod (memegen.rs). NOT triggered by tags.
git tag v0.1.0 && git push origin v0.1.0   # -> release.yml: versioned GHCR image + git-cliff GitHub Release + ClawHub skill
```

Release notes come from Conventional Commit messages via git-cliff (`.github/cliff.toml`) - so commit hygiene *is* the changelog. The GHCR package is private on first push; flip it public once for the Release's `docker pull` link to work anonymously.

## Deploy architecture (Cloudflare)

Worker on custom domain `memegen.rs` -> single-instance **Container** running the Rust server (binds `0.0.0.0:5005`, `getContainer` singleton, `max_instances: 1`, scales to zero after 10m idle).

- Edge caching is Workers Caching (`cache.enabled` in `wrangler.jsonc`), tiered across PoPs with request collapsing; cache HITs never invoke the Worker or the container. TTLs come from the Rust server's headers: images/assets send `Cache-Control: max-age=86400` (browsers) + `CDN-Cache-Control: immutable` (edge); HTML/JSON send none and get the 2h heuristic. The cache key includes the Worker version, so every deploy busts it.
- Render throttle is a **Cloudflare edge rate limiter** (`RENDER_LIMITER`, 10000/60s aggregate per location) - a runaway-bill backstop, not a per-user limit. Only cache-miss renders on `/images/` count. There is no in-app limiter.
- Config: `ops/worker/wrangler.jsonc`. Worker bindings -> `ops/worker/worker-configuration.d.ts` via `wrangler types` (generated, do not hand-edit).
- Analytics: a `worker.ts` HTMLRewriter injects the `EXTRA_HTML_SCRIPTS` wrangler var into each HTML `<head>`. It loads `/mesh/script.js`, served by a **separate `memegen-rybbit-proxy` Worker (not in this repo)** that proxies to a self-hosted Rybbit instance. No `/mesh` code lives here.

## Gotchas

- Cloudflare Containers **cannot pull from GHCR** - CI copies the image into `registry.cloudflare.com` (which also forbids the `latest` tag; a commit-derived tag is used).
- Containers require `linux/amd64`.
- A template whose only background is an undecodable `default.mp4` is listed but returns `422` on render.
- 403s in prod come from Cloudflare zone-level bot protection, not app code (`worker.ts` has zero UA logic).

## URL scheme

`/images/{id}/{line1}/{line2}.{png|jpg|webp|gif}` - lines split on `/`, space = `_`, literal underscore = `__`, blank line = `_`. Query params: `style`, `layout=top`, `width`/`height` (blurred letterbox), `color`. Custom background: `/images/custom/{lines}.png?background=<url>`.

## Code style

- **Minimal comments.** Comment *why*, not *what*; the code is the documentation. Don't narrate obvious lines. (The existing config files - `worker.ts`, `wrangler.jsonc` - carry dense rationale comments on purpose because the deploy behavior is non-obvious; match that bar only where the reasoning is genuinely load-bearing.)
- **Code cleanliness / minimalism.** Every new file must justify its existence - if it can be inlined, inline it. Split only for a functional reason (different lifecycle/runtime), never for "organization". No reference/template/example files. Start from the fewest files that work. This repo is deliberately ~1800 LOC across 3 Rust files; keep it that way.
- Read code before making claims about it; never guess a flag - check `--help`.
- Don't edit/implement until asked; when intent is ambiguous, research and recommend rather than act.
- ASCII-only symbols in docs; single `-` hyphens, never em/en dashes.
- Standalone docs (plans, design notes) get a `YYMM-DD-<name>.md` date prefix.

## Conventions

- **Conventional Commits** (`feat(scope): ...`, `fix:`, `chore:`, ...). New commits, never amend unless asked. No `--no-verify`, no force-push.
- Template image licensing is deliberate: code is MIT, bundled meme images are not (nominative/transformative use, same posture as memegen.link). See README "License".
