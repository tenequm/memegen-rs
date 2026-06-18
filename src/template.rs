use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
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
    /// Legacy / alternate slugs that resolve to this template (old IDs preserved
    /// after the canonical-slug migration so existing URLs keep working).
    #[serde(default, deserialize_with = "string_list")]
    pub(crate) aliases: Vec<String>,
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
    /// Overall popularity rank (1 = most popular) sourced from imgflip; `None`
    /// for templates with no popularity signal (they sort after ranked ones).
    #[serde(skip)]
    pub(crate) rank: Option<u32>,
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
    /// Alternate-slug -> canonical-id lookup, populated from each template's
    /// `aliases` list so legacy IDs resolve via [`Registry::get`].
    aliases: HashMap<String, String>,
    /// Canonical IDs in display order: ranked templates first (ascending rank),
    /// then the unranked tail alphabetically.
    order: Vec<String>,
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

        // Apply popularity ranks from the sidecar file (keyed by canonical slug).
        for (slug, rank) in load_popularity(&dir.join("popularity.json")) {
            if let Some(t) = templates.get_mut(&slug) {
                t.rank = Some(rank);
            }
        }

        // Build the alias -> canonical lookup. A canonical ID always wins over an
        // alias that happens to collide with it.
        let mut aliases = HashMap::new();
        for (id, t) in &templates {
            for alias in &t.aliases {
                if !templates.contains_key(alias) {
                    aliases.insert(alias.clone(), id.clone());
                }
            }
        }

        // Precompute display order: ranked first (ascending), then alphabetical.
        let mut order: Vec<String> = templates.keys().cloned().collect();
        order.sort_by(|a, b| match (templates[a].rank, templates[b].rank) {
            (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.cmp(b)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a.cmp(b),
        });

        Ok(Self {
            templates,
            aliases,
            order,
        })
    }

    pub(crate) fn get(&self, id: &str) -> Option<&Template> {
        self.templates
            .get(id)
            .or_else(|| self.aliases.get(id).and_then(|c| self.templates.get(c)))
    }

    pub(crate) fn all(&self) -> impl Iterator<Item = &Template> {
        self.order.iter().filter_map(|id| self.templates.get(id))
    }

    pub(crate) fn len(&self) -> usize {
        self.templates.len()
    }
}

/// Read the popularity sidecar (`{ "<slug>": { "rank": N, .. }, .. }`) and return
/// `(slug, rank)` pairs. A missing or malformed file is non-fatal: the registry
/// simply falls back to alphabetical ordering.
fn load_popularity(path: &Path) -> Vec<(String, u32)> {
    #[derive(Deserialize)]
    struct Pop {
        rank: Option<u32>,
    }
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    match serde_json::from_str::<HashMap<String, Pop>>(&raw) {
        Ok(map) => map
            .into_iter()
            .filter_map(|(slug, p)| p.rank.map(|r| (slug, r)))
            .collect(),
        Err(e) => {
            eprintln!("ignoring {}: {e}", path.display());
            Vec::new()
        }
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

    fn registry() -> Registry {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
        Registry::load(&dir).expect("load templates")
    }

    #[test]
    fn legacy_ids_resolve_via_alias() {
        let reg = registry();
        // Old cryptic IDs were renamed to canonical imgflip slugs but kept as aliases.
        assert_eq!(
            reg.get("db").map(|t| t.id.as_str()),
            Some("distracted-boyfriend")
        );
        assert_eq!(reg.get("fry").map(|t| t.id.as_str()), Some("futurama-fry"));
        // A merged duplicate's old ID resolves to the surviving canonical template.
        assert_eq!(
            reg.get("burning-house-girl").map(|t| t.id.as_str()),
            Some("disaster-girl")
        );
        // Canonical lookups still work.
        assert!(reg.get("drake-hotline-bling").is_some());
    }

    #[test]
    fn popularity_orders_listing() {
        let reg = registry();
        let order: Vec<_> = reg.all().collect();
        assert_eq!(order[0].id, "drake-hotline-bling", "rank 1 sorts first");
        // Ranked templates are contiguous at the front in ascending rank order,
        // and every unranked template follows them.
        let mut prev = 0u32;
        let mut seen_unranked = false;
        for t in &order {
            match t.rank {
                Some(r) => {
                    assert!(
                        !seen_unranked,
                        "{} (ranked) appears after an unranked",
                        t.id
                    );
                    assert!(r >= prev, "ranks out of order at {}", t.id);
                    prev = r;
                }
                None => seen_unranked = true,
            }
        }
    }
}
