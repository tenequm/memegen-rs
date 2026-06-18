---
name: memegen-rs
description: Generate meme images from memegen.rs by building a URL. The template and caption text go straight into the URL path, and the response is the image (PNG or animated GIF). Use when asked to make a meme, caption an image, or produce a shareable meme link.
metadata:
  version: "0.1.0"
  openclaw:
    homepage: https://github.com/tenequm/memegen-rs
    emoji: "🖼️"
    os: [darwin, linux]
---

# memegen-rs

Generate memes by building a URL against `https://memegen.rs`. The response is the image itself - nothing to install, no account, no request body. The same URL always returns the same image, so a meme link is stable and shareable.

## Build a captioned meme

`https://memegen.rs/images/{template}/{line1}/{line2}.png`

Encode the caption text into each path segment:

- Separate caption lines with `/`.
- Replace each space with `_`.
- Replace a literal underscore with `__`.
- Use `_` alone for a blank line.

## Examples

- Two-line Drake meme:
  `https://memegen.rs/images/drake/writing_a_parser/just_using_a_url.png`
- "X everywhere" (Buzz), top and bottom:
  `https://memegen.rs/images/buzz/memes/memes_everywhere.png`
- Blank template, no caption:
  `https://memegen.rs/images/drake.png`
- Caption any image on the web:
  `https://memegen.rs/images/custom/one_does_not_simply/use_a_database.png?background=https://example.com/photo.jpg`
- Animated GIF output (templates whose source is an animated GIF):
  `https://memegen.rs/images/fry/not_sure_if_static/or_animated.gif`

## Options (query parameters)

- `style` - alternate background variant for the template
- `layout=top` - place all captions at the top
- `width`, `height` - pad to a fixed size with a blurred letterbox
- `color` - text fill color (name or hex)

## Find templates

- Every template with its id, name, line count, and an example URL: `GET https://memegen.rs/templates`
- A single template: `GET https://memegen.rs/templates/{id}`

## Full reference

The complete, machine-readable contract - every endpoint, parameter, and schema - is the OpenAPI spec: `https://memegen.rs/openapi.json`.

## Tip

Return the meme as a clickable link rather than pasting the raw URL into chat.
