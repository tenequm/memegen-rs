# memegen-rs

[![standard-readme compliant](https://img.shields.io/badge/readme%20style-standard-brightgreen.svg?style=flat-square)](https://github.com/RichardLitt/standard-readme)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.95%2B-orange.svg?style=flat-square)](https://www.rust-lang.org)

A minimal, stateless meme generator HTTP API in pure Rust.

Every meme is described entirely by its URL - there is no database, no cache server, and nothing to log in to. Backgrounds and caption geometry come from a directory of template folders, and each request renders an image on demand. It is a deliberately small reimplementation of [jacebrowning/memegen](https://github.com/jacebrowning/memegen): three endpoint groups, embedded fonts, no SaaS plumbing - around 1,000 lines across three source files.

## Table of Contents

- [Background](#background)
- [Install](#install)
- [Usage](#usage)
- [Templates](#templates)
- [Architecture](#architecture)
- [Scope](#scope)
- [API](#api)
- [Maintainers](#maintainers)
- [Acknowledgements](#acknowledgements)
- [Contributing](#contributing)
- [License](#license)

## Background

The original [memegen](https://github.com/jacebrowning/memegen) by Jace Browning is an excellent, long-running meme API: stateless, URL-as-state, with a large template corpus. It is written in Python (Sanic + Pillow) and carries a fair amount of hosted-service machinery - authentication, remote tracking, search, error reporting.

`memegen-rs` keeps the parts that make memegen elegant (the statelessness, the URL scheme, the `config.yml` + `default.*` template format) and drops everything that only exists to run it as a public SaaS. The result is a single self-contained binary with a tiny dependency surface, intended for self-hosting. It reads the **same template layout** as upstream, so an existing corpus drops in unchanged.

## Install

Requires Rust 1.95+ (pinned via `rust-toolchain.toml`).

```sh
git clone https://github.com/tenequm/memegen-rs.git
cd memegen-rs
cargo build --release
```

The binary is then at `target/release/memegen-rs`. The template corpus ships in `templates/` (see [Templates](#templates)), so it runs out of the box.

## Usage

```sh
cargo run                  # serves on http://0.0.0.0:5005
# or the release binary:
./target/release/memegen-rs
```

Environment variables:

| Variable | Default | Purpose |
|---|---|---|
| `PORT` | `5005` | Listen port |
| `MEMEGEN_TEMPLATES_DIR` | `templates` | Path to the template corpus |

Render a meme:

```sh
# captioned: lines are /-separated, space is _, blank line is _
curl 'http://localhost:5005/images/fry/not_sure_if/this_works.png' -o meme.png

# any image as a background
curl 'http://localhost:5005/images/custom/top/bottom.png?background=https://picsum.photos/600/400' -o custom.png

# sized (padded with a blurred letterbox) and recolored
curl 'http://localhost:5005/images/ds/push_button/cant_decide.jpg?width=800&height=600&color=yellow' -o ds.jpg
```

## Templates

A template is a folder named by its ID:

```
templates/<id>/
  config.yml      # name, source, keywords, text-box geometry, example
  default.png     # background (png/jpg/webp/gif; first frame used for gif)
```

`config.yml` uses the same schema as upstream memegen, so any memegen-compatible corpus works. Text-box coordinates are fractions of the image (0.0-1.0). Alternate background variants (extra image files beside `default.*`) become selectable via `?style=<name>`.

This repository ships a corpus of ~780 templates under `templates/`, read once at startup into an immutable in-memory registry. Add or replace templates by dropping folders in, or point `MEMEGEN_TEMPLATES_DIR` at a different directory. See [License](#license) for the licensing posture on the bundled images.

## Architecture

- **[axum](https://crates.io/crates/axum)** for HTTP routing, **[utoipa](https://crates.io/crates/utoipa)** for the OpenAPI spec (no Swagger UI).
- **[image](https://crates.io/crates/image)** + **[imageproc](https://crates.io/crates/imageproc)** + **[ab_glyph](https://crates.io/crates/ab_glyph)** for rendering. A caption is autosized to its box, word-wrapped, drawn with a white fill and a black outline, and composited onto the background.
- **[serde-saphyr](https://crates.io/crates/serde-saphyr)** (pure-Rust YAML) parses each `config.yml`.
- Fonts (**[Anton](https://fonts.google.com/specimen/Anton)** for the Impact look, **[Kalam](https://fonts.google.com/specimen/Kalam)** for handwriting; both SIL OFL 1.1) are embedded in the binary via `include_bytes!` - no font directory, and it works on a fonts-less container.

Three source files: `template.rs` (model, registry, URL codec, styling), `render.rs` (the rendering pipeline), `main.rs` (router, handlers, OpenAPI, error mapping).

## Scope

Image output in `png`, `jpg`, `webp`, and `gif` - animated when the template's source is an animated GIF, otherwise a single frame. Per-template font selection (Anton plus a Kalam handwriting face) and text rotation are honored.

Still out of scope, layerable without restructuring:

- Animated WebP / MP4 output
- Overlay-image compositing
- Color emoji and the full upstream font set

Templates whose only background is an undecodable video (`default.mp4` with no static still) are listed in the API but return `422` on render.

## API

| Method | Path | Description |
|---|---|---|
| `GET` | `/templates` | List all templates (JSON) |
| `GET` | `/templates/{id}` | One template (JSON) |
| `GET` | `/images/{id}.{ext}` | Blank template background |
| `GET` | `/images/{id}/{lines}.{ext}` | Captioned meme |
| `GET` | `/images/custom/{lines}.{ext}?background=<url>` | Caption any image by URL |
| `GET` | `/openapi.json` | OpenAPI 3.1 spec |

Path encoding for `{lines}`: lines are separated by `/`; a space is `_`; a literal underscore is `__`; a blank line is `_`.

Query parameters for image endpoints:

| Param | Effect |
|---|---|
| `style` | Alternate background variant |
| `layout=top` | Place all captions at the top |
| `width`, `height` | Pad to size with a blurred letterbox |
| `color` | Text fill color (name or hex) |

The full machine-readable contract is served at `/openapi.json`.

## Maintainers

[@tenequm](https://github.com/tenequm)

## Acknowledgements

This project is a reimplementation of [**memegen**](https://github.com/jacebrowning/memegen) by [Jace Browning](https://github.com/jacebrowning) ([memegen.link](https://memegen.link)). The URL scheme, the template format, and the overall API shape are his design; this repository simply rebuilds a minimal subset of it in Rust. All credit for the original idea and the template ecosystem goes to him and the memegen contributors.

Also thanks to the [Anton](https://github.com/googlefonts/AntonFont) and [Kalam](https://github.com/googlefonts/kalam) typefaces (SIL OFL 1.1) and the maintainers of [axum](https://github.com/tokio-rs/axum), [image](https://github.com/image-rs/image), [imageproc](https://github.com/image-rs/imageproc), and [ab_glyph](https://github.com/alexheretic/ab-glyph).

## Contributing

Issues and pull requests are welcome - open an [issue](https://github.com/tenequm/memegen-rs/issues) for bugs or ideas. Before submitting a PR, please run `cargo fmt`, `cargo clippy`, and `cargo test`.

## License

Code, build scripts, and template `config.yml` markup are [MIT](LICENSE) (c) 2026 Misha Kolesnik. The embedded [Anton](assets/OFL-Anton.txt) and [Kalam](assets/OFL-Kalam.txt) fonts are SIL OFL 1.1.

### Template images

The MIT license does **not** extend to the template background images and clips shipped under `templates/`. These are well-known internet meme formats whose underlying photos, film stills, and artwork are owned by their respective copyright holders. They are included in good faith for the same nominative/transformative use that meme-generation tools rely on (the same legal posture as [memegen.link](https://memegen.link) and the upstream memegen image), and **no claim of ownership is made over them**.

If you are a rights holder and want a template removed, email **misha@kolesnik.io** with the template name (its folder under `templates/`) and proof of rights. Removal requests are honored promptly - typically within a few days.

For zero redistribution exposure, remove `templates/` and rely on the `?background=<url>` custom-background endpoint instead.
