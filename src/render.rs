use std::fs::File;
use std::io::{BufReader, Cursor};
use std::path::Path;
use std::sync::LazyLock;

use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, Frame, ImageFormat, Rgba, RgbaImage, imageops};
use imageproc::geometric_transformations::{Border, Interpolation, rotate_about_center};

use crate::template::{Template, stylize};

static ANTON: LazyLock<FontArc> =
    LazyLock::new(|| load_font(include_bytes!("../assets/Anton-Regular.ttf")));
static PANGOLIN: LazyLock<FontArc> =
    LazyLock::new(|| load_font(include_bytes!("../assets/Pangolin-Regular.ttf")));
// Neutral sans used only for the watermark (Manrope variable, default weight).
static MANROPE: LazyLock<FontArc> =
    LazyLock::new(|| load_font(include_bytes!("../assets/Manrope-Regular.ttf")));

fn load_font(bytes: &'static [u8]) -> FontArc {
    FontArc::try_from_slice(bytes).expect("valid font")
}

/// Optional bottom-left brand label, read once from `MEMEGEN_WATERMARK`.
/// Unset or blank means no watermark (the default).
static WATERMARK: LazyLock<Option<String>> = LazyLock::new(|| {
    std::env::var("MEMEGEN_WATERMARK")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
});

/// Caption autosizing defaults. A horizontal safe-margin keeps text off the
/// edges, and a cap on font size (as a fraction of image height) stops short
/// one- or two-word captions from ballooning to fill the whole text band.
const SAFE_MARGIN: f32 = 0.05; // each side, fraction of the box width
const MAX_FONT_FRAC: f32 = 0.14; // fraction of image height

fn font(name: &str) -> &'static FontArc {
    match name {
        "comic" | "kalam" => &PANGOLIN,
        _ => &ANTON,
    }
}

#[derive(Debug)]
pub(crate) enum RenderError {
    NoBackground,
    Unsupported(String),
    Decode(String),
    Encode(String),
}

pub(crate) struct Spec<'a> {
    pub(crate) lines: &'a [String],
    pub(crate) ext: &'a str,
    pub(crate) size: (u32, u32),
    pub(crate) style: &'a str,
    pub(crate) layout: &'a str,
    pub(crate) color: Option<&'a str>,
}

pub(crate) fn render(
    template: &Template,
    spec: &Spec,
) -> Result<(Vec<u8>, &'static str), RenderError> {
    if spec.ext == "gif"
        && let Some(gif) = template.animated_source(spec.style)
        && let Some(out) = animated_gif(&gif, template, spec)?
    {
        return Ok((out, "image/gif"));
    }
    let bg = template
        .background(spec.style)
        .ok_or(RenderError::NoBackground)?;
    // Decode by content, not file extension: a handful of corpus files carry a
    // `.jpg` name but PNG bytes (and vice versa), which `image::open` (which
    // picks the decoder from the extension) rejects.
    let bytes = std::fs::read(&bg).map_err(|e| RenderError::Decode(e.to_string()))?;
    let img = image::load_from_memory(&bytes)
        .map_err(|e| RenderError::Decode(e.to_string()))?
        .to_rgba8();
    let boxes = effective_boxes(template, spec.layout, spec.lines.len());
    finish(img, &boxes, spec)
}

pub(crate) fn render_custom(
    bytes: &[u8],
    spec: &Spec,
) -> Result<(Vec<u8>, &'static str), RenderError> {
    let img = image::load_from_memory(bytes)
        .map_err(|e| RenderError::Decode(e.to_string()))?
        .to_rgba8();
    let boxes = if spec.layout == "top" {
        top_boxes(spec.lines.len())
    } else {
        default_boxes()
    };
    finish(img, &boxes, spec)
}

/// A pre-rendered caption layer and where to composite it on the background.
struct Caption {
    layer: RgbaImage,
    x: i64,
    y: i64,
}

fn finish(
    mut img: RgbaImage,
    boxes: &[Box],
    spec: &Spec,
) -> Result<(Vec<u8>, &'static str), RenderError> {
    let captions = caption_layers(img.dimensions(), boxes, spec);
    overlay_captions(&mut img, &captions);
    if spec.size.0 > 0 && spec.size.1 > 0 {
        img = pad_to(&img, spec.size.0, spec.size.1);
    }
    draw_watermark(&mut img);
    encode(img, spec.ext)
}

/// Build each caption layer once for a given canvas size - the layout, stroke,
/// and rotation are frame-invariant, so this is computed once and reused.
fn caption_layers(size: (u32, u32), boxes: &[Box], spec: &Spec) -> Vec<Caption> {
    let (w, h) = (size.0 as f32, size.1 as f32);
    boxes
        .iter()
        .enumerate()
        .filter_map(|(i, b)| {
            let caption = stylize(&b.style, spec.lines.get(i)?);
            if caption.trim().is_empty() {
                return None;
            }
            let color = spec
                .color
                .and_then(|c| c.split(',').next())
                .map(parse_color)
                .unwrap_or_else(|| parse_color(&b.color));
            let box_w = b.scale_x * w;
            let inset = SAFE_MARGIN * box_w;
            let rect = Rect {
                x: b.anchor_x * w + inset,
                y: b.anchor_y * h,
                w: (box_w - 2.0 * inset).max(1.0),
                h: b.scale_y * h,
            };
            Some(build_caption(
                font(&b.font),
                &caption,
                rect,
                &b.align,
                b.angle,
                color,
                MAX_FONT_FRAC * h,
            ))
        })
        .collect()
}

fn overlay_captions(img: &mut RgbaImage, captions: &[Caption]) {
    for c in captions {
        imageops::overlay(img, &c.layer, c.x, c.y);
    }
}

/// Draw the `MEMEGEN_WATERMARK` label bottom-left as a small brand mark (soft
/// dark halo behind a faded white label, so it stays legible on any background),
/// echoing upstream memegen. No-op when unset, and skipped on thumbnail-sized
/// output where a height-proportional label would dominate.
fn draw_watermark(img: &mut RgbaImage) {
    let Some(label) = WATERMARK.as_deref() else {
        return;
    };
    let (w, h) = img.dimensions();
    if w.min(h) < 200 {
        return;
    }
    let f = &MANROPE;
    let px = (h as f32 * 0.024).max(11.0);
    let line_h = f.as_scaled(PxScale::from(px)).height();
    let text_w = measure(f, label, px);
    let blur = (px * 0.11).max(1.4);
    let pad = (blur * 2.0).ceil() as i32 + 2;
    let lw = (text_w.ceil() as i32 + 2 * pad).max(1) as u32;
    let lh = (line_h.ceil() as i32 + 2 * pad).max(1) as u32;

    // A soft dark halo (blurred, kept near full strength) is what keeps the mark
    // legible on light *and* dark backgrounds - the industry-standard fix. On a
    // dark background the halo just blends in, so strengthening it only helps the
    // bright-background case. Only the white label is faded, to stay subtle.
    let mut layer = RgbaImage::new(lw, lh);
    draw_line(&mut layer, f, Rgba([0, 0, 0, 255]), pad, pad, px, label);
    let mut layer = imageops::fast_blur(&layer, blur);
    for p in layer.pixels_mut() {
        p.0[3] = ((p.0[3] as f32 * 2.2) as u32).min(225) as u8;
    }
    let mut text = RgbaImage::new(lw, lh);
    draw_line(
        &mut text,
        f,
        Rgba([255, 255, 255, 255]),
        pad,
        pad,
        px,
        label,
    );
    for p in text.pixels_mut() {
        p.0[3] = (p.0[3] as f32 * 0.92) as u8;
    }
    imageops::overlay(&mut layer, &text, 0, 0);

    let m = (h as f32 * 0.016).max(6.0);
    let x = (m - pad as f32).round() as i64;
    let y = (h as f32 - m - line_h - pad as f32).round() as i64;
    imageops::overlay(img, &layer, x, y);
}

fn animated_gif(
    gif: &Path,
    template: &Template,
    spec: &Spec,
) -> Result<Option<Vec<u8>>, RenderError> {
    let decode = |e: image::ImageError| RenderError::Decode(e.to_string());
    let file = File::open(gif).map_err(|e| RenderError::Decode(e.to_string()))?;
    let frames = GifDecoder::new(BufReader::new(file))
        .map_err(decode)?
        .into_frames()
        .collect_frames()
        .map_err(decode)?;
    if frames.len() <= 1 {
        return Ok(None);
    }
    let boxes = effective_boxes(template, spec.layout, spec.lines.len());
    let captions = caption_layers(frames[0].buffer().dimensions(), &boxes, spec);
    let mut out = Cursor::new(Vec::new());
    {
        let encode = |e: image::ImageError| RenderError::Encode(e.to_string());
        let mut enc = GifEncoder::new(&mut out);
        enc.set_repeat(Repeat::Infinite).map_err(encode)?;
        for fr in frames {
            let (left, top, delay) = (fr.left(), fr.top(), fr.delay());
            let mut buf = fr.into_buffer();
            overlay_captions(&mut buf, &captions);
            draw_watermark(&mut buf);
            enc.encode_frame(Frame::from_parts(buf, left, top, delay))
                .map_err(encode)?;
        }
    }
    Ok(Some(out.into_inner()))
}

struct Box {
    style: String,
    color: String,
    font: String,
    angle: f32,
    anchor_x: f32,
    anchor_y: f32,
    scale_x: f32,
    scale_y: f32,
    align: String,
}

fn effective_boxes(template: &Template, layout: &str, lines: usize) -> Vec<Box> {
    if layout == "top" {
        return top_boxes(lines);
    }
    template
        .text
        .iter()
        .map(|t| Box {
            style: t.style.clone(),
            color: t.color.clone(),
            font: t.font.clone(),
            angle: t.angle,
            anchor_x: t.anchor_x,
            anchor_y: t.anchor_y,
            scale_x: t.scale_x,
            scale_y: t.scale_y,
            align: t.align.clone(),
        })
        .collect()
}

fn top_boxes(lines: usize) -> Vec<Box> {
    let n = lines.max(1) as f32;
    (0..lines.max(1))
        .map(|i| Box {
            style: "none".into(),
            color: "white".into(),
            font: String::new(),
            angle: 0.0,
            anchor_x: 0.0,
            anchor_y: i as f32 * (0.2 / n),
            scale_x: 1.0,
            scale_y: 0.2 / n,
            align: "center".into(),
        })
        .collect()
}

fn default_boxes() -> Vec<Box> {
    [0.0, 0.8]
        .map(|y| Box {
            style: "upper".into(),
            color: "white".into(),
            font: String::new(),
            angle: 0.0,
            anchor_x: 0.0,
            anchor_y: y,
            scale_x: 1.0,
            scale_y: 0.2,
            align: "center".into(),
        })
        .into()
}

struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

fn build_caption(
    f: &FontArc,
    caption: &str,
    rect: Rect,
    align: &str,
    angle: f32,
    fill: Rgba<u8>,
    max_px: f32,
) -> Caption {
    let (px, lines) = layout_text(f, caption, rect.w, rect.h, max_px);
    let line_h = f.as_scaled(PxScale::from(px)).height();
    let total_h = line_h * lines.len() as f32;
    let block_top = (rect.h - total_h).max(0.0) / 2.0;
    let stroke = (px / 12.0).round().clamp(1.0, 4.0) as i32;
    let outline = stroke_color(fill);

    // Pad the layer by the stroke width so the outline isn't clipped at the box
    // edge (and rotation has room to spread).
    let margin = stroke as f32;
    let mut layer = RgbaImage::new(
        (rect.w.max(1.0) + 2.0 * margin) as u32,
        (rect.h.max(1.0) + 2.0 * margin) as u32,
    );
    for (i, line) in lines.iter().enumerate() {
        let line_w = measure(f, line, px);
        let x = margin
            + match align {
                "left" => 0.0,
                "right" => rect.w - line_w,
                _ => (rect.w - line_w) / 2.0,
            };
        let y = margin + block_top + i as f32 * line_h;
        let (xi, yi) = (x.round() as i32, y.round() as i32);

        for dy in -stroke..=stroke {
            for dx in -stroke..=stroke {
                if dx * dx + dy * dy <= stroke * stroke {
                    draw_line(&mut layer, f, outline, xi + dx, yi + dy, px, line);
                }
            }
        }
        draw_line(&mut layer, f, fill, xi, yi, px, line);
    }

    if angle.abs() > f32::EPSILON {
        layer = rotate_about_center(
            &layer,
            -angle.to_radians(),
            Interpolation::Bilinear,
            Border::Constant(Rgba([0, 0, 0, 0])),
        );
    }
    Caption {
        layer,
        x: (rect.x - margin) as i64,
        y: (rect.y - margin) as i64,
    }
}

fn draw_line(
    img: &mut RgbaImage,
    f: &FontArc,
    color: Rgba<u8>,
    x: i32,
    y: i32,
    px: f32,
    text: &str,
) {
    imageproc::drawing::draw_text_mut(img, color, x, y, PxScale::from(px), f, text);
}

/// Find the largest font size where the word-wrapped caption fits the box.
fn layout_text(
    f: &FontArc,
    caption: &str,
    box_w: f32,
    box_h: f32,
    max_px: f32,
) -> (f32, Vec<String>) {
    let mut px = box_h.min(max_px).max(8.0);
    loop {
        let lines = wrap(f, caption, px, box_w);
        let line_h = f.as_scaled(PxScale::from(px)).height();
        let total_h = line_h * lines.len() as f32;
        let widest = lines.iter().map(|l| measure(f, l, px)).fold(0.0, f32::max);
        if (total_h <= box_h && widest <= box_w) || px <= 8.0 {
            return (px, lines);
        }
        px -= 2.0;
    }
}

fn wrap(f: &FontArc, caption: &str, px: f32, box_w: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in caption.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if measure(f, &candidate, px) <= box_w || current.is_empty() {
            current = candidate;
        } else {
            lines.push(std::mem::take(&mut current));
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn measure(f: &FontArc, text: &str, px: f32) -> f32 {
    let scaled = f.as_scaled(PxScale::from(px));
    let mut width = 0.0;
    let mut prev = None;
    for ch in text.chars() {
        let glyph = f.glyph_id(ch);
        if let Some(p) = prev {
            width += scaled.kern(p, glyph);
        }
        width += scaled.h_advance(glyph);
        prev = Some(glyph);
    }
    width
}

fn pad_to(img: &RgbaImage, w: u32, h: u32) -> RgbaImage {
    let ratio = (w as f32 / img.width() as f32).min(h as f32 / img.height() as f32);
    let nw = ((img.width() as f32 * ratio).round() as u32).max(1);
    let nh = ((img.height() as f32 * ratio).round() as u32).max(1);
    let fitted = imageops::resize(img, nw, nh, FilterType::Lanczos3);

    let cover = imageops::resize(img, w, h, FilterType::Triangle);
    let mut canvas = imageops::fast_blur(&cover, 16.0);
    imageops::overlay(
        &mut canvas,
        &fitted,
        ((w - nw) / 2) as i64,
        ((h - nh) / 2) as i64,
    );
    canvas
}

/// Decode a background and produce a small square cover-cropped thumbnail.
///
/// The gallery cards display these at ~170 CSS px; we render at 2x for crisp
/// HiDPI. Two costs matter and they're separate:
///   - decode: the browser re-decodes every `<img>` on each navigation, and the
///     raw corpus backgrounds run to 1440px, so a grid of full-res sources
///     flashes its placeholders before the bitmaps land. Downscaling to ~340px
///     (~13x fewer pixels) is what kills that flash, regardless of codec.
///   - bytes: we encode JPEG, not WebP. `image`'s WebP encoder (image-webp) is
///     lossless-only, which on a photo is ~90-140KB - no real win over the
///     source. The cover-crop fills the square fully so there's no alpha to
///     keep, and lossy JPEG lands the same thumb in ~10-20KB.
pub(crate) fn thumbnail(bytes: &[u8], size: u32) -> Result<(Vec<u8>, &'static str), RenderError> {
    use fast_image_resize::images::Image as FirImage;
    use fast_image_resize::{PixelType, ResizeAlg, ResizeOptions, Resizer};

    // Decode by content, not extension (same corpus quirk as `render`).
    let img = image::load_from_memory(bytes)
        .map_err(|e| RenderError::Decode(e.to_string()))?
        .to_rgba8();
    let (w, h) = img.dimensions();
    let to_err = |e: fast_image_resize::ResizeError| RenderError::Decode(e.to_string());

    // `object-fit: cover` for a square target: crop the source to its centered
    // square (fir does the crop as part of the resize), then scale that to
    // `size`. One SIMD Lanczos3 pass instead of resize-then-crop.
    let side = w.min(h) as f64;
    let src = FirImage::from_vec_u8(w, h, img.into_raw(), PixelType::U8x4)
        .map_err(|e| RenderError::Decode(e.to_string()))?;
    let mut dst = FirImage::new(size, size, PixelType::U8x4);
    Resizer::new()
        .resize(
            &src,
            &mut dst,
            &ResizeOptions::new()
                .resize_alg(ResizeAlg::Convolution(
                    fast_image_resize::FilterType::Lanczos3,
                ))
                .crop((w as f64 - side) / 2.0, (h as f64 - side) / 2.0, side, side),
        )
        .map_err(to_err)?;

    let cropped = RgbaImage::from_raw(size, size, dst.into_vec())
        .ok_or_else(|| RenderError::Encode("thumbnail buffer size mismatch".into()))?;
    encode(cropped, "jpg")
}

fn encode(img: RgbaImage, ext: &str) -> Result<(Vec<u8>, &'static str), RenderError> {
    let dynimg = DynamicImage::ImageRgba8(img);
    let mut buf = Cursor::new(Vec::new());
    let mime = match ext {
        "png" => {
            write(&dynimg, &mut buf, ImageFormat::Png)?;
            "image/png"
        }
        "jpg" | "jpeg" => {
            write(
                &DynamicImage::ImageRgb8(dynimg.to_rgb8()),
                &mut buf,
                ImageFormat::Jpeg,
            )?;
            "image/jpeg"
        }
        "webp" => {
            write(&dynimg, &mut buf, ImageFormat::WebP)?;
            "image/webp"
        }
        "gif" => {
            write(&dynimg, &mut buf, ImageFormat::Gif)?;
            "image/gif"
        }
        other => return Err(RenderError::Unsupported(other.to_string())),
    };
    Ok((buf.into_inner(), mime))
}

fn write(
    img: &DynamicImage,
    buf: &mut Cursor<Vec<u8>>,
    format: ImageFormat,
) -> Result<(), RenderError> {
    img.write_to(buf, format)
        .map_err(|e| RenderError::Encode(e.to_string()))
}

fn stroke_color(fill: Rgba<u8>) -> Rgba<u8> {
    let [r, g, b, _] = fill.0;
    let luma = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    if luma > 110.0 {
        Rgba([0, 0, 0, 255])
    } else {
        Rgba([255, 255, 255, 255])
    }
}

fn parse_color(s: &str) -> Rgba<u8> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex(hex);
    }
    let named = match s.to_lowercase().as_str() {
        "black" => [0, 0, 0],
        "white" => [255, 255, 255],
        "red" => [255, 0, 0],
        "green" => [0, 128, 0],
        "blue" => [0, 0, 255],
        "yellow" => [255, 255, 0],
        "orange" => [255, 165, 0],
        "purple" => [128, 0, 128],
        "pink" => [255, 192, 203],
        "gray" | "grey" => [128, 128, 128],
        "cyan" => [0, 255, 255],
        "magenta" => [255, 0, 255],
        _ => [255, 255, 255],
    };
    Rgba([named[0], named[1], named[2], 255])
}

fn parse_hex(hex: &str) -> Rgba<u8> {
    let expand = |s: &str| u8::from_str_radix(s, 16).unwrap_or(255);
    match hex.len() {
        3 => Rgba([
            expand(&hex[0..1].repeat(2)),
            expand(&hex[1..2].repeat(2)),
            expand(&hex[2..3].repeat(2)),
            255,
        ]),
        6 => Rgba([
            expand(&hex[0..2]),
            expand(&hex[2..4]),
            expand(&hex[4..6]),
            255,
        ]),
        8 => Rgba([
            expand(&hex[0..2]),
            expand(&hex[2..4]),
            expand(&hex[4..6]),
            expand(&hex[6..8]),
        ]),
        _ => Rgba([255, 255, 255, 255]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Guards against a font swap silently dropping Cyrillic; the Ukrainian
    // letters i/yi/ye/ghe-upturn are the ones generic Cyrillic fonts miss.
    #[test]
    fn embedded_fonts_cover_ukrainian() {
        for f in [&*ANTON, &*PANGOLIN] {
            for c in ['А', 'я', 'і', 'ї', 'є', 'ґ', 'І', 'Ї', 'Є', 'Ґ'] {
                assert_ne!(f.glyph_id(c).0, 0, "missing glyph for {c:?}");
            }
        }
    }
}
