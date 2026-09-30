# AGENTS.md - memegen-rs

Stateless meme-generator HTTP API + web UI in **pure Rust**. Every meme is fully described by its URL: no DB, no cache server, no login. A minimal Rust reimplementation of [jacebrowning/memegen](https://github.com/jacebrowning/memegen). Production: <https://memegen.rs>.

## Layout

- `src/template.rs` - model, in-memory registry (read once at startup), URL codec, styling.
- `src/render.rs` - rendering pipeline (autosize, wrap, outline, composite, GIF encode).
- `src/main.rs` - axum router, handlers, OpenAPI, error mapping, web UI (maud, compile-time).
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

### Smoke test (against a running `:5005`)

```sh
curl -sf 'http://localhost:5005/images/drake/writing_a_parser/just_using_a_url.png' -o /tmp/m.png  # render
curl -sf 'http://localhost:5005/templates' | head                                                  # registry JSON
curl -sf 'http://localhost:5005/' -o /dev/null && echo ok                                           # web UI / docs
```

Prod smoke test: same paths against `https://memegen.rs`.

## Releasing a new version

Building an image, rolling it out and cutting a versioned release are **separate** steps. CI never deploys:

```sh
git push origin main      # -> image.yml builds and pushes ghcr.io/tenequm/memegen-rs:sha-<commit> (+ latest). Does NOT change production.
git tag v0.1.0 && git push origin v0.1.0   # -> release.yml: versioned GHCR image + git-cliff GitHub Release + ClawHub skill
```

Production runs whatever image is pinned by digest in a private infra repo's helmfile. To roll a commit out, wait for `image.yml` to finish, then bump the pin there to `sha-<commit>@<digest>` and apply it:

```sh
crane digest ghcr.io/tenequm/memegen-rs:sha-<commit>   # full 40-char commit SHA -> sha256:...
```

Release notes come from Conventional Commit messages via git-cliff (`.github/cliff.toml`) - so commit hygiene *is* the changelog. The GHCR package is private on first push; flip it public once for the Release's `docker pull` link to work anonymously.

## Deploy architecture (Kubernetes)

One pod on a Kubernetes cluster runs the Rust server (binds `0.0.0.0:5005`) from `ghcr.io/tenequm/memegen-rs`, pinned by digest in a private infra repo's helmfile. No manifests live in this repo. As of 2026-09-30 `memegen.rs` is still served by the previously deployed Cloudflare Worker and container, which nothing in this repo updates any more, until its DNS is pointed at the cluster.

- Limits: 2 CPU, 512 MiB. The pod runs non-root with a read-only root filesystem.
- There is no edge cache and no rate limiter, at the edge or in the app. The pod's CPU limit is the flood backstop.
- The server fetches any URL given as `?background=` with no filtering of its own. On the cluster a NetworkPolicy limits that fetch to the public internet.

## Gotchas

- The image is built for `linux/amd64` only (the cluster's nodes).
- A template whose only background is an undecodable `default.mp4` is listed but returns `422` on render.

## URL scheme

`/images/{id}/{line1}/{line2}.{png|jpg|webp|gif}` - lines split on `/`, space = `_`, literal underscore = `__`, blank line = `_`. Query params: `style`, `layout=top`, `width`/`height` (blurred letterbox), `color`. Custom background: `/images/custom/{lines}.png?background=<url>`.

## Code style

- **Minimal comments.** Comment *why*, not *what*; the code is the documentation. Don't narrate obvious lines. Write a dense rationale comment only where the reasoning is genuinely load-bearing.
- **Code cleanliness / minimalism.** Every new file must justify its existence - if it can be inlined, inline it. Split only for a functional reason (different lifecycle/runtime), never for "organization". No reference/template/example files. Start from the fewest files that work. This repo is deliberately ~1800 LOC across 3 Rust files; keep it that way.
- Read code before making claims about it; never guess a flag - check `--help`.
- Don't edit/implement until asked; when intent is ambiguous, research and recommend rather than act.
- ASCII-only symbols in docs; single `-` hyphens, never em/en dashes.
- Standalone docs (plans, design notes) get a `YYMM-DD-<name>.md` date prefix.

## Conventions

- **Conventional Commits** (`feat(scope): ...`, `fix:`, `chore:`, ...). New commits, never amend unless asked. No `--no-verify`, no force-push.
- Template image licensing is deliberate: code is MIT, bundled meme images are not (nominative/transformative use, same posture as memegen.link). See README "License".
