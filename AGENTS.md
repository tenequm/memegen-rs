# AGENTS.md - memegen-rs

Stateless meme-generator HTTP API + web UI in **pure Rust**. Every meme is fully described by its URL: no DB, no cache server, no login. A minimal Rust reimplementation of [jacebrowning/memegen](https://github.com/jacebrowning/memegen). Production: <https://memegen.rs>.

## Layout

- `src/template.rs` - model, in-memory registry (read once at startup), URL codec, styling.
- `src/render.rs` - rendering pipeline (autosize, wrap, outline, composite, GIF encode).
- `src/cache.rs` - optional render cache: a middleware on the two template image routes, backed by foyer. Off unless `MEMEGEN_CACHE_DIR` is set.
- `src/main.rs` - axum router, handlers, OpenAPI, error mapping, web UI (maud, compile-time).
- `ops/docker/` - `Containerfile` + `Containerfile.dockerignore` for the container image build.
- `templates/<id>/` - 700 template folders, each `config.yml` (upstream memegen schema) + `default.{png,jpg,webp,gif}`. `templates/popularity.json` ranks them. **Committed to the repo and baked into the image.**
- `assets/` - embedded fonts (Anton, Pangolin, and Manrope for the watermark; SIL OFL), favicons, OG image, `SKILL.md` (the ClawHub agent skill; `SKILL.md` at root is a symlink to it). `Anton-Regular.ttf` is the Cyrillic-extended v2.300 build from [Tural/AntonFont](https://github.com/Tural/AntonFont) (unmerged upstream as [google/fonts#7552](https://github.com/google/fonts/issues/7552)); Anton and Pangolin cover Latin + full Cyrillic/Ukrainian.

## Stack

axum (HTTP) + utoipa/Scalar (`/docs`, `/openapi.json`) + maud (compile-time UI) + image/imageproc/ab_glyph (render) + serde-saphyr (pure-Rust YAML) + fast_image_resize (SIMD thumbnails) + reqwest/rustls (`?background=` fetch) + foyer (render cache). Fonts embedded via `include_bytes!`. Edition 2024, Rust 1.95 (pinned in `rust-toolchain.toml`).

## Dev loop

```sh
cargo run                 # local server on :5005, reads ./templates (override: MEMEGEN_TEMPLATES_DIR)
cargo fmt && cargo clippy && cargo test   # validate; run before every commit
```

Lints are strict (`unsafe_code = forbid`, clippy `all = deny`), but CI only builds the image - nothing there runs fmt, clippy or tests, so run them yourself. `cargo clippy` does **not** emit a binary; rebuild with `cargo run`/`cargo build` before smoke-testing or you'll hit a stale executable.

Environment variables, each read once: `PORT` (default `5005`), `MEMEGEN_TEMPLATES_DIR` (default `templates`), `MEMEGEN_WATERMARK` (brand label on renders; unset = none), `MEMEGEN_HEAD_HTML` (HTML appended verbatim to the `<head>` of every page - gallery, builder, `/docs`; trusted operator config, never escaped; unset or empty = pages unchanged).

### Smoke test (against a running `:5005`)

```sh
curl -sf 'http://localhost:5005/images/drake/writing_a_parser/just_using_a_url.png' -o /tmp/m.png  # render
curl -sf 'http://localhost:5005/templates' | head                                                  # registry JSON
curl -sf 'http://localhost:5005/' -o /dev/null && echo ok                                           # web UI / docs
```

Prod smoke test: same paths against `https://memegen.rs`, which is the pod described under "Deploy architecture". Template image responses there carry `x-memegen-cache: hit|miss`.

## Releasing a new version

Building an image, rolling it out and cutting a versioned release are **separate** steps:

```sh
git push origin main      # -> image.yml builds and pushes ghcr.io/tenequm/memegen-rs:sha-<commit> (+ latest). Rolls nothing out.
git tag v0.1.0 && git push origin v0.1.0   # -> release.yml: versioned GHCR image + git-cliff GitHub Release + ClawHub skill
```

The cluster runs whatever image is pinned by digest in a private infra repo's helmfile. To roll a commit out, wait for `image.yml` to finish, then bump the pin there to `sha-<commit>@<digest>` and apply it:

```sh
crane digest ghcr.io/tenequm/memegen-rs:sha-<commit>   # full 40-char commit SHA -> sha256:...
```

Release notes come from Conventional Commit messages via git-cliff (`.github/cliff.toml`) - so commit hygiene *is* the changelog.

## Deploy architecture (Kubernetes)

One pod on a Kubernetes cluster runs the Rust server (binds `0.0.0.0:5005`) from the pinned `ghcr.io/tenequm/memegen-rs` image. No manifests live in this repo.

- Limits: 2 CPU, 1 GiB. The pod runs non-root with a read-only root filesystem; the cache volume is the only path it can write.
- Render cache: on, with `MEMEGEN_CACHE_DIR=/cache` on an `emptyDir` and the default 10 GiB bound (see "Render cache"). The volume goes with the pod and the server reads nothing back from it at startup, so every rollout or restart starts empty.
- Cloudflare hosts the DNS zone and nothing else: no Worker, no edge cache, no rate limiter and no bot protection in the request path, only the cluster's ingress. The app has no rate limiter either - it runs at most one render per core and queues the rest - so the pod's CPU and memory limits are the flood backstop.
- The server fetches any URL given as `?background=` with no filtering of its own. On the cluster a NetworkPolicy limits that fetch to the public internet.
- Images and assets send `Cache-Control: max-age=86400` and `CDN-Cache-Control: immutable` (one year). `/manifest.webmanifest` (a day) and `/SKILL.md` / `/llms.txt` (an hour) send `Cache-Control` only; HTML pages and JSON send neither. Nothing in front of the pod reads the CDN header.
- Analytics: the tag is whatever `MEMEGEN_HEAD_HTML` holds, appended by the server to each page `<head>`. Nothing outside the pod injects it.

`memegen.rs` has been served by this pod since its DNS was pointed at the cluster on 2026-09-30; until then a Cloudflare Worker and container served it, and their code left this repo in #4.

## Gotchas

- The image is built for `linux/amd64` only (the cluster's nodes).
- A template whose only background is an undecodable `default.mp4` is listed but returns `422` on render.
- Per-request limits are constants, not env vars: in `render.rs` `MAX_SIDE` (2048 px, for `width`/`height` and for the size a custom background is drawn at), `MAX_BACKGROUND_SIDE` (4096 px, the size one may arrive with) and `MAX_TOP_LINES` (32); in `main.rs` `MAX_BACKGROUND_BYTES` (10 MiB). Over a size or byte limit is a `422`; a background between the two sides is shrunk to `MAX_SIDE`, and lines past `MAX_TOP_LINES` are dropped. README "API" lists them for callers. A custom background is decoded only if it is a PNG, JPEG, GIF or WebP: the `image` crate's other decoders allocate outside the limits it is given. Template backgrounds are trusted and not limited, so the largest ones in the corpus set the memory a render can take.

## URL scheme

`/images/{id}/{line1}/{line2}.{png|jpg|webp|gif}` - lines split on `/`, space = `_`, literal underscore = `__`, blank line = `_`. Query params: `style`, `layout=top`, `width`/`height` (blurred letterbox), `color`. Custom background: `/images/custom/{lines}.png?background=<url>`.

## Render cache

Off by default; with `MEMEGEN_CACHE_DIR` unset the server behaves exactly as without it. Set, it caches 200s of `GET /images/{id}/{*text}` and `GET /images/{filename}`, keyed by the request path plus raw query string, and adds `x-memegen-cache: hit|miss`. `/images/custom/...` and non-GETs never reach the cache; a non-200 is answered as the handler gave it and never stored. Identical concurrent requests render once and share the answer, whatever its status (foyer's `get_or_fetch`).

| Variable | Default | Purpose |
|---|---|---|
| `MEMEGEN_CACHE_DIR` | _(unset)_ | Cache directory; the only path the server writes to |
| `MEMEGEN_CACHE_MAX_BYTES` | `10737418240` (10 GiB) | Disk bound, at least 1 MiB |
| `MEMEGEN_CACHE_MEMORY_BYTES` | `16777216` (16 MiB) | Memory tier; `0` keeps only the newest render in memory |

- **The disk bound is structural.** At startup foyer creates at most `max_bytes / block` sparse files of `block = min(64 MiB, max_bytes / 8)` bytes each, holds every one open, and only ever writes inside them, so the files sum to at most `MEMEGEN_CACHE_MAX_BYTES` (160 files of 64 MiB at the default). Nothing else is written; the only overhead on top is the filesystem's own metadata. Eviction reclaims the oldest block whole, one block ahead of the writer.
- **Startup does not check free space.** The files are sparse, so a volume smaller than the bound starts fine and fails its writes once it fills. foyer reports those through `tracing`, which nothing here subscribes to, so the only symptom is renders that stay misses. Keep `MEMEGEN_CACHE_MAX_BYTES` below the volume's size with some headroom for filesystem metadata - an `emptyDir` that outgrows its `sizeLimit` gets the pod evicted.
- **The directory must be on a real disk and writable by the server alone.** On tmpfs (`emptyDir.medium: Memory`, many `/tmp`) every cached byte is memory charged to the container. Block files are opened by name and trusted on their checksum, so a directory someone else can write to can redirect the writes or plant responses.
- **A render larger than one block, or than foyer's 16 MiB write buffer, is served but never reaches disk.** Disk writes are best-effort too: under a burst foyer drops what does not fit its write buffer, and that render is simply a miss next time.
- **The cache is empty on every process start** (`RecoverMode::None`), because a render also depends on the templates and `MEMEGEN_WATERMARK`, which are not in the key. Nothing invalidates an entry while the process runs, so a template edited in place keeps serving its old render until restart. A directory with old content is fine, but lowering `MEMEGEN_CACHE_MAX_BYTES` against a reused directory leaves the old, larger set of block files behind - wipe it. A Kubernetes `emptyDir` never hits this.
- **Memory.** The memory tier holds at most `MEMEGEN_CACHE_MEMORY_BYTES` of renders (or one render, if that is larger), counting 1 KiB of bookkeeping per entry. On top, foyer keeps two 16 MiB write buffers (touched only as far as a batch fills them), up to 16 MiB of renders queued for disk plus up to 32 MiB in the batches being written, a read buffer and a decoded copy per in-flight disk hit, and an index of roughly 50-90 bytes per render on disk. Entries are page-aligned on disk, so the index is a few MiB for ordinary renders but about 2% of `MEMEGEN_CACHE_MAX_BYTES` if every render is tiny (some 200 MiB at the 10 GiB default; measured 49 MiB for 535k one-pixel renders) - on a small pod, size the bound with that in mind. A memory hit is only 0.2-1 ms faster than a disk hit, so the tier is not worth growing.
- **Set `MALLOC_MMAP_THRESHOLD_=131072` wherever the cache is on (Linux/glibc).** Measured in the container over 1000 mixed renders: RSS settles about 30 MiB above the uncached server with it, about 100 MiB above without it - glibc's default heap holds on to the freed render-sized buffers. It costs nothing measurable per render.
- An unusable directory or an unparsable bound fails startup rather than silently running uncached.
- A miss renders in a task of its own (foyer spawns the fetch), so it finishes and is stored even if the client disconnects - and a request that has been accepted always renders, client or no client.
- The memory tier hands renders to disk when it evicts them, so a fresh render shows up in the directory only once newer ones push it out.

## Code style

- **Minimal comments.** Comment *why*, not *what*; the code is the documentation. Don't narrate obvious lines. Write a dense rationale comment only where the reasoning is genuinely load-bearing.
- **Code cleanliness / minimalism.** Every new file must justify its existence - if it can be inlined, inline it. Split only for a functional reason (different lifecycle/runtime), never for "organization". No reference/template/example files. Start from the fewest files that work. This repo is deliberately ~2700 lines (tests included) across 4 Rust files; keep it that way.
- Read code before making claims about it; never guess a flag - check `--help`.
- Don't edit/implement until asked; when intent is ambiguous, research and recommend rather than act.
- ASCII-only symbols in docs; single `-` hyphens, never em/en dashes.
- Standalone docs (plans, design notes) get a `YYMM-DD-<name>.md` date prefix.

## Conventions

- **Conventional Commits** (`feat(scope): ...`, `fix:`, `chore:`, ...). New commits, never amend unless asked. No `--no-verify`, no force-push.
- Template image licensing is deliberate: code is MIT, bundled meme images are not (nominative/transformative use, same posture as memegen.link). See README "License".
