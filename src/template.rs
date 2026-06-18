use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer};

const STATIC_EXTS: [&str; 5] = ["png", "jpg", "jpeg", "webp", "gif"];

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TextBox {
    #[serde(default = "default_style")]
    pub(crate) style: String,
    #[serde(default = "default_color")]
    pub(crate) color: String,
    #[serde(default)]
    pub(crate) font: String,
    #[serde(default)]
    pub(crate) anchor_x: f32,
    #[serde(default)]
    pub(crate) anchor_y: f32,
    #[serde(default)]
    pub(crate) angle: f32,
    #[serde(default = "one")]
    pub(crate) scale_x: f32,
    #[serde(default = "default_scale_y")]
    pub(crate) scale_y: f32,
    #[serde(default = "default_align")]
    pub(crate) align: String,
}

fn default_style() -> String {
    "upper".into()
}
fn default_color() -> String {
    "white".into()
}
fn default_align() -> String {
    "center".into()
}
fn one() -> f32 {
    1.0
}
fn default_scale_y() -> f32 {
    0.2
}

/// Deserialize a YAML list whose items may be null (the corpus uses bare `-`
/// entries), mapping null to an empty string and preserving positions.
fn string_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Vec::<Option<String>>::deserialize(deserializer)?;
    Ok(raw.into_iter().map(Option::unwrap_or_default).collect())
}

fn default_text() -> Vec<TextBox> {
    vec![
        TextBox {
            style: default_style(),
            color: default_color(),
            font: String::new(),
            anchor_x: 0.0,
            anchor_y: 0.0,
            angle: 0.0,
            scale_x: 1.0,
            scale_y: 0.2,
            align: default_align(),
        },
        TextBox {
            style: default_style(),
            color: default_color(),
            font: String::new(),
            anchor_x: 0.0,
            anchor_y: 0.8,
            angle: 0.0,
            scale_x: 1.0,
            scale_y: 0.2,
            align: default_align(),
        },
    ]
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Template {
    #[serde(skip)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) source: Option<String>,
    #[serde(default, deserialize_with = "string_list")]
    pub(crate) keywords: Vec<String>,
    #[serde(default = "default_text")]
    pub(crate) text: Vec<TextBox>,
    #[serde(default, deserialize_with = "string_list")]
    pub(crate) example: Vec<String>,
    #[serde(skip)]
    pub(crate) dir: PathBuf,
    #[serde(skip)]
    pub(crate) styles: Vec<String>,
    #[serde(skip)]
    pub(crate) is_gif: bool,
    #[serde(skip)]
    pub(crate) default_background: Option<PathBuf>,
}

impl Template {
    pub(crate) fn lines(&self) -> usize {
        self.text.len()
    }

    /// First static-decodable background file for the given style, falling back
    /// to the default background. Returns `None` when only an undecodable source
    /// (e.g. a bare `default.mp4`) exists - the template is metadata-only in v1.
    pub(crate) fn background(&self, style: &str) -> Option<PathBuf> {
        let style = if style.is_empty() { "default" } else { style };
        if let Some(p) = self.find_background(style) {
            return Some(p);
        }
        if style != "default" {
            return self.find_background("default");
        }
        None
    }

    fn find_background(&self, stem: &str) -> Option<PathBuf> {
        STATIC_EXTS
            .iter()
            .map(|ext| self.dir.join(format!("{stem}.{ext}")))
            .find(|p| p.is_file())
    }

    /// The animated `.gif` source for a style, if one exists (used for animated
    /// output; `background` would otherwise prefer a static still).
    pub(crate) fn animated_source(&self, style: &str) -> Option<PathBuf> {
        let style = if style.is_empty() { "default" } else { style };
        [
            self.dir.join(format!("{style}.gif")),
            self.dir.join("default.gif"),
        ]
        .into_iter()
        .find(|p| p.is_file())
    }

    /// A render URL that is guaranteed valid for smoke tests.
    pub(crate) fn example_path(&self) -> String {
        let slug = if self.example.iter().any(|l| !l.is_empty()) {
            encode(&self.example)
        } else {
            "_".into()
        };
        format!("/images/{}/{slug}.png", self.id)
    }

    pub(crate) fn blank_path(&self) -> String {
        format!("/images/{}.png", self.id)
    }
}

pub(crate) struct Registry {
    templates: BTreeMap<String, Template>,
}

impl Registry {
    pub(crate) fn load(dir: &Path) -> anyhow::Result<Self> {
        let mut templates = BTreeMap::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if !path.is_dir() {
                continue;
            }
            let Some(id) = path
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if id.starts_with('_') || id == "custom" {
                continue;
            }
            let config = path.join("config.yml");
            if !config.is_file() {
                continue;
            }
            let raw = std::fs::read_to_string(&config)?;
            let mut template: Template = match serde_saphyr::from_str(&raw) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("skipping {id}: bad config.yml: {e}");
                    continue;
                }
            };
            template.id = id.clone();
            template.styles = list_styles(&path);
            template.dir = path;
            // Resolve the filesystem probes once; both are immutable per template
            // and sit on hot, unthrottled paths (thumbnails, gallery cards).
            template.is_gif = template.animated_source("default").is_some();
            template.default_background = template.background("default");
            templates.insert(id, template);
        }
        if templates.is_empty() {
            anyhow::bail!("no templates loaded from {}", dir.display());
        }
        Ok(Self { templates })
    }

    pub(crate) fn get(&self, id: &str) -> Option<&Template> {
        self.templates.get(id)
    }

    pub(crate) fn all(&self) -> impl Iterator<Item = &Template> {
        self.templates.values()
    }

    pub(crate) fn len(&self) -> usize {
        self.templates.len()
    }
}

fn list_styles(dir: &Path) -> Vec<String> {
    let mut styles: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            let (Some(stem), Some(ext)) = (
                p.file_stem().and_then(|s| s.to_str()),
                p.extension().and_then(|s| s.to_str()),
            ) else {
                continue;
            };
            if stem == "default" || stem == "config" || stem.starts_with('_') {
                continue;
            }
            if STATIC_EXTS.contains(&ext) {
                styles.push(stem.to_string());
            }
        }
    }
    styles.sort();
    styles.dedup();
    styles
}

/// Apply a text box's casing style to a caption.
pub(crate) fn stylize(style: &str, text: &str) -> String {
    match style {
        "upper" => text.to_uppercase(),
        "lower" => text.to_lowercase(),
        "mock" => mock(text),
        _ => text.to_string(),
    }
}

fn mock(text: &str) -> String {
    text.chars()
        .enumerate()
        .map(|(i, c)| {
            if i % 2 == 0 {
                c.to_ascii_lowercase()
            } else {
                c.to_ascii_uppercase()
            }
        })
        .collect()
}

const UNDERSCORE_SENTINEL: char = '\u{0}';

/// Encode caption lines into a URL path slug: lines joined by `/`, space <-> `_`,
/// literal underscore as `__`, blank as `_`. URL-reserved characters get
/// upstream-memegen-compatible `~` escapes so a caption can safely contain them
/// (notably `?`, which would otherwise start the query string, and `/`, which
/// would otherwise read as a line separator).
pub(crate) fn encode(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| {
            if line.is_empty() {
                "_".to_string()
            } else {
                line.replace('_', "__")
                    .replace(' ', "_")
                    .replace('?', "~q")
                    .replace('&', "~a")
                    .replace('%', "~p")
                    .replace('#', "~h")
                    .replace('/', "~s")
                    .replace('"', "''")
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Decode a URL path slug into caption lines (inverse of `encode`).
pub(crate) fn decode(slug: &str) -> Vec<String> {
    slug.split('/')
        .map(|seg| {
            if seg == "_" {
                String::new()
            } else {
                seg.replace("__", &UNDERSCORE_SENTINEL.to_string())
                    .replace('_', " ")
                    .replace(UNDERSCORE_SENTINEL, "_")
                    .replace("~q", "?")
                    .replace("~a", "&")
                    .replace("~p", "%")
                    .replace("~h", "#")
                    .replace("~s", "/")
                    .replace("''", "\"")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_round_trips() {
        for lines in [
            vec!["hello world".to_string(), "second line".to_string()],
            vec!["one_two".to_string()],
            vec![String::new(), "bottom only".to_string()],
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
            vec!["for the better, right?".to_string()],
            vec!["50% off & more".to_string(), "a/b #1".to_string()],
            vec!["say \"hi\"".to_string()],
        ] {
            assert_eq!(
                decode(&encode(&lines)),
                lines,
                "round trip failed for {lines:?}"
            );
        }
    }

    #[test]
    fn decode_spaces_and_underscores() {
        assert_eq!(decode("not_sure_if"), vec!["not sure if"]);
        assert_eq!(decode("a__b"), vec!["a_b"]);
        assert_eq!(decode("top/bottom"), vec!["top", "bottom"]);
        assert_eq!(decode("_"), vec![""]);
    }

    #[test]
    fn styles_casing() {
        assert_eq!(stylize("upper", "hi there"), "HI THERE");
        assert_eq!(stylize("lower", "HI"), "hi");
        assert_eq!(stylize("none", "Mixed Case"), "Mixed Case");
        assert_eq!(stylize("mock", "abcd"), "aBcD");
    }
}
