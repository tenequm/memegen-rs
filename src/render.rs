use std::io::Cursor;
use std::sync::LazyLock;

use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage, imageops};

use crate::template::{Template, stylize};

static FONT: LazyLock<FontArc> = LazyLock::new(|| {
    FontArc::try_from_slice(include_bytes!("../assets/Anton-Regular.ttf")).expect("valid font")
});

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

fn finish(
    mut img: RgbaImage,
    boxes: &[Box],
    spec: &Spec,
) -> Result<(Vec<u8>, &'static str), RenderError> {
    let (w, h) = (img.width() as f32, img.height() as f32);
    for (i, b) in boxes.iter().enumerate() {
        let Some(raw) = spec.lines.get(i) else {
            continue;
        };
        let caption = stylize(&b.style, raw);
        if caption.trim().is_empty() {
            continue;
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
        draw_caption(&mut img, &caption, rect, &b.align, color);
    }

    if spec.size.0 > 0 && spec.size.1 > 0 {
        img = pad_to(&img, spec.size.0, spec.size.1);
    }
    encode(img, spec.ext)
}

struct Box {
    style: String,
    color: String,
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
            anchor_x: 0.0,
            anchor_y: i as f32 * (0.2 / n),
            scale_x: 1.0,
            scale_y: 0.2 / n,
            align: "center".into(),
        })
        .collect()
}

fn default_boxes() -> Vec<Box> {
    vec![
        Box {
            style: "upper".into(),
            color: "white".into(),
            anchor_x: 0.0,
            anchor_y: 0.0,
            scale_x: 1.0,
            scale_y: 0.2,
            align: "center".into(),
        },
        Box {
            style: "upper".into(),
            color: "white".into(),
            anchor_x: 0.0,
            anchor_y: 0.8,
            scale_x: 1.0,
            scale_y: 0.2,
            align: "center".into(),
        },
    ]
}

struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

fn draw_caption(img: &mut RgbaImage, caption: &str, rect: Rect, align: &str, fill: Rgba<u8>) {
    let (px, lines) = layout_text(caption, rect.w, rect.h);
    let scaled = FONT.as_scaled(PxScale::from(px));
    let line_h = scaled.height();
    let total_h = line_h * lines.len() as f32;
    let block_top = rect.y + (rect.h - total_h).max(0.0) / 2.0;
    let stroke = (px / 12.0).round().clamp(1.0, 4.0) as i32;
    let outline = stroke_color(fill);

    for (i, line) in lines.iter().enumerate() {
        let line_w = measure(line, px);
        let x = match align {
            "left" => rect.x,
            "right" => rect.x + rect.w - line_w,
            _ => rect.x + (rect.w - line_w) / 2.0,
        };
        let y = block_top + i as f32 * line_h;
        let (xi, yi) = (x.round() as i32, y.round() as i32);

        for dy in -stroke..=stroke {
            for dx in -stroke..=stroke {
                if dx * dx + dy * dy <= stroke * stroke {
                    draw_line(img, outline, xi + dx, yi + dy, px, line);
                }
            }
        }
        draw_line(img, fill, xi, yi, px, line);
    }
}

fn draw_line(img: &mut RgbaImage, color: Rgba<u8>, x: i32, y: i32, px: f32, text: &str) {
    imageproc::drawing::draw_text_mut(img, color, x, y, PxScale::from(px), &*FONT, text);
}

/// Find the largest font size where the word-wrapped caption fits the box.
fn layout_text(caption: &str, box_w: f32, box_h: f32) -> (f32, Vec<String>) {
    let mut px = box_h.max(8.0);
    loop {
        let lines = wrap(caption, px, box_w);
        let line_h = FONT.as_scaled(PxScale::from(px)).height();
        let total_h = line_h * lines.len() as f32;
        let widest = lines.iter().map(|l| measure(l, px)).fold(0.0, f32::max);
        if (total_h <= box_h && widest <= box_w) || px <= 8.0 {
            return (px, lines);
        }
        px -= 2.0;
    }
}

fn wrap(caption: &str, px: f32, box_w: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in caption.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if measure(&candidate, px) <= box_w || current.is_empty() {
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

fn measure(text: &str, px: f32) -> f32 {
    let scaled = FONT.as_scaled(PxScale::from(px));
    let mut width = 0.0;
    let mut prev = None;
    for ch in text.chars() {
        let glyph = FONT.glyph_id(ch);
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
