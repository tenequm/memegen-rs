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
static KALAM: LazyLock<FontArc> =
    LazyLock::new(|| load_font(include_bytes!("../assets/Kalam-Regular.ttf")));

fn load_font(bytes: &'static [u8]) -> FontArc {
    FontArc::try_from_slice(bytes).expect("valid font")
}

fn font(name: &str) -> &'static FontArc {
    match name {
        "comic" | "kalam" => &KALAM,
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
    let img = image::open(&bg)
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
            let rect = Rect {
                x: b.anchor_x * w,
                y: b.anchor_y * h,
                w: b.scale_x * w,
                h: b.scale_y * h,
            };
            Some(build_caption(
                font(&b.font),
                &caption,
                rect,
                &b.align,
                b.angle,
                color,
            ))
        })
        .collect()
}

fn overlay_captions(img: &mut RgbaImage, captions: &[Caption]) {
    for c in captions {
        imageops::overlay(img, &c.layer, c.x, c.y);
    }
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
) -> Caption {
    let (px, lines) = layout_text(f, caption, rect.w, rect.h);
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
fn layout_text(f: &FontArc, caption: &str, box_w: f32, box_h: f32) -> (f32, Vec<String>) {
    let mut px = box_h.max(8.0);
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
