mod render;
mod template;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use maud::{DOCTYPE, Markup, PreEscaped, html};
use serde::{Deserialize, Serialize};
use utoipa::{OpenApi, ToSchema};
use utoipa_scalar::{Scalar, Servable};

use render::{RenderError, Spec};
use template::{Registry, Template, decode};

type AppState = Arc<Registry>;

/// Rendered memes are fully described by their URL, so the same URL is the same
/// image forever - safe to cache hard at the browser and the Cloudflare edge.
const IMMUTABLE: &str = "public, max-age=31536000, s-maxage=31536000, immutable";

/// Canonical origin for absolute URLs in social-share metadata (og:url,
/// og:image) - link crawlers reject relative URLs.
const SITE: &str = "https://memegen.rs";

/// Homepage share card: a meme rendered by the app itself, padded to the
/// universal 1200x630 social size as JPEG (edge-cached like any render).
const BRAND_OG: &str =
    "https://memegen.rs/images/buzz/memes/memes_everywhere.jpg?width=1200&height=630";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = std::env::var("MEMEGEN_TEMPLATES_DIR").unwrap_or_else(|_| "templates".into());
    let registry = Arc::new(Registry::load(&PathBuf::from(&dir))?);
    println!("loaded {} templates from {dir}", registry.len());

    // Rate limiting is enforced at the edge (Cloudflare Worker, see worker.ts),
    // not here - the Worker rejects render abuse before it reaches this origin.
    let app = Router::new()
        .route("/", get(gallery))
        .route("/edit/{id}", get(builder))
        .route("/thumbs/{id}", get(thumb))
        .route("/font/anton.ttf", get(anton_font))
        .route("/favicon.ico", get(favicon_ico))
        .route("/favicon.svg", get(favicon_svg))
        .route("/apple-touch-icon.png", get(apple_touch_icon))
        .route("/icon-192.png", get(icon_192))
        .route("/icon-512.png", get(icon_512))
        .route("/manifest.webmanifest", get(manifest))
        .route("/openapi.json", get(openapi))
        .route("/templates", get(list_templates))
        .route("/templates/{id}", get(get_template))
        .route("/images/custom/{*text}", get(render_custom))
        .route("/images/{id}/{*text}", get(render_text))
        .route("/images/{filename}", get(render_blank))
        // Scalar API docs (rendered from the OpenAPI spec).
        .merge(Scalar::with_url("/docs", ApiDoc::openapi()))
        // /SKILL.md, /llms.txt, and any-case variants serve the embedded doc.
        .fallback(docs_fallback)
        .with_state(registry);

    let port = std::env::var("PORT").unwrap_or_else(|_| "5005".into());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    println!("listening on http://0.0.0.0:{port}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Exit promptly on SIGTERM (and Ctrl-C) so a Cloudflare Containers rollout
/// replaces this instance immediately instead of waiting out the graceful
/// shutdown window.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

// ---------- DTOs ----------

#[derive(Serialize, ToSchema)]
struct ExampleDto {
    text: Vec<String>,
    url: String,
}

#[derive(Serialize, ToSchema)]
struct TemplateDto {
    id: String,
    name: String,
    lines: usize,
    styles: Vec<String>,
    source: Option<String>,
    keywords: Vec<String>,
    blank: String,
    example: ExampleDto,
}

fn to_dto(t: &Template) -> TemplateDto {
    TemplateDto {
        id: t.id.clone(),
        name: t.name.clone(),
        lines: t.lines(),
        styles: t.styles.clone(),
        source: t.source.clone(),
        keywords: t.keywords.clone(),
        blank: t.blank_path(),
        example: ExampleDto {
            text: t.example.clone(),
            url: t.example_path(),
        },
    }
}

// ---------- API handlers ----------

async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

#[utoipa::path(get, path = "/templates", responses((status = 200, body = [TemplateDto])))]
async fn list_templates(State(reg): State<AppState>) -> Json<Vec<TemplateDto>> {
    Json(reg.all().map(to_dto).collect())
}

#[utoipa::path(
    get,
    path = "/templates/{id}",
    params(("id" = String, Path, description = "Template ID")),
    responses((status = 200, body = TemplateDto), (status = 404))
)]
async fn get_template(
    State(reg): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TemplateDto>, AppError> {
    reg.get(&id)
        .map(|t| Json(to_dto(t)))
        .ok_or_else(|| AppError::NotFound(format!("template not found: {id}")))
}

#[derive(Deserialize)]
struct ImgQuery {
    style: Option<String>,
    layout: Option<String>,
    color: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    background: Option<String>,
}

impl ImgQuery {
    fn size(&self) -> (u32, u32) {
        (self.width.unwrap_or(0), self.height.unwrap_or(0))
    }
    fn style(&self) -> &str {
        self.style.as_deref().unwrap_or("default")
    }
    fn layout(&self) -> &str {
        self.layout.as_deref().unwrap_or("default")
    }
}

#[utoipa::path(
    get,
    path = "/images/{id}/{text}",
    params(
        ("id" = String, Path, description = "Template ID"),
        ("text" = String, Path, description = "Caption lines and extension, e.g. top/bottom.png")
    ),
    responses((status = 200, description = "Rendered meme", content_type = "image/*"))
)]
async fn render_text(
    State(reg): State<AppState>,
    Path((id, text)): Path<(String, String)>,
    Query(q): Query<ImgQuery>,
) -> Result<Response, AppError> {
    let (slug, ext) = split_ext(&text);
    let template = reg
        .get(&id)
        .ok_or_else(|| AppError::NotFound(format!("template not found: {id}")))?;
    let lines = decode(slug);
    let spec = Spec {
        lines: &lines,
        ext,
        size: q.size(),
        style: q.style(),
        layout: q.layout(),
        color: q.color.as_deref(),
    };
    let (bytes, mime) = render::render(template, &spec).map_err(AppError::from)?;
    Ok(cached_bytes(bytes, mime))
}

#[utoipa::path(
    get,
    path = "/images/{filename}",
    params(("filename" = String, Path, description = "Template ID and extension, e.g. fry.png")),
    responses((status = 200, description = "Blank template", content_type = "image/*"))
)]
async fn render_blank(
    State(reg): State<AppState>,
    Path(filename): Path<String>,
    Query(q): Query<ImgQuery>,
) -> Result<Response, AppError> {
    let (id, ext) = split_ext(&filename);
    let template = reg
        .get(id)
        .ok_or_else(|| AppError::NotFound(format!("template not found: {id}")))?;
    let spec = Spec {
        lines: &[],
        ext,
        size: q.size(),
        style: q.style(),
        layout: q.layout(),
        color: q.color.as_deref(),
    };
    let (bytes, mime) = render::render(template, &spec).map_err(AppError::from)?;
    Ok(cached_bytes(bytes, mime))
}

#[utoipa::path(
    get,
    path = "/images/custom/{text}",
    params(
        ("text" = String, Path, description = "Caption lines and extension"),
        ("background" = String, Query, description = "Source image URL")
    ),
    responses((status = 200, description = "Rendered meme on a custom background", content_type = "image/*"))
)]
async fn render_custom(
    Path(text): Path<String>,
    Query(q): Query<ImgQuery>,
) -> Result<Response, AppError> {
    let url = q
        .background
        .clone()
        .ok_or_else(|| AppError::BadRequest("background URL is required".into()))?;
    let (slug, ext) = split_ext(&text);
    let lines = decode(slug);
    let bytes = fetch(&url).await?;
    let spec = Spec {
        lines: &lines,
        ext,
        size: q.size(),
        style: "default",
        layout: q.layout(),
        color: q.color.as_deref(),
    };
    let (out, mime) = render::render_custom(&bytes, &spec).map_err(AppError::from)?;
    Ok(cached_bytes(out, mime))
}

// ---------- Static-ish handlers (unthrottled) ----------

/// Serve a template's blank background straight from disk - no rendering. This
/// is what makes a gallery of hundreds of thumbnails viable under the 5/s render
/// cap: thumbnails never touch the renderer.
async fn thumb(State(reg): State<AppState>, Path(id): Path<String>) -> Result<Response, AppError> {
    let template = reg
        .get(&id)
        .ok_or_else(|| AppError::NotFound(format!("template not found: {id}")))?;
    let path = template
        .default_background
        .as_ref()
        .ok_or_else(|| AppError::NotFound(format!("no background for {id}")))?;
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(cached_bytes(bytes, mime_for(path)))
}

/// The same Anton face the memes are captioned in, served as a webfont so the UI
/// headlines match the rendered output. Embedded at compile time (the binary
/// already ships it), so it works on a fonts-less container.
async fn anton_font() -> Response {
    static_asset(include_bytes!("../assets/Anton-Regular.ttf"), "font/ttf")
}

// Favicon set (2026 minimal): .ico + svg + apple-touch 180 + 192/512 PNGs +
// manifest. All embedded at compile time and served immutable.
async fn favicon_ico() -> Response {
    static_asset(include_bytes!("../assets/favicon.ico"), "image/x-icon")
}
async fn favicon_svg() -> Response {
    static_asset(include_bytes!("../assets/favicon.svg"), "image/svg+xml")
}
async fn apple_touch_icon() -> Response {
    static_asset(
        include_bytes!("../assets/apple-touch-icon.png"),
        "image/png",
    )
}
async fn icon_192() -> Response {
    static_asset(include_bytes!("../assets/icon-192.png"), "image/png")
}
async fn icon_512() -> Response {
    static_asset(include_bytes!("../assets/icon-512.png"), "image/png")
}

async fn manifest() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        MANIFEST,
    )
        .into_response()
}

const MANIFEST: &str = r##"{
  "name": "memegen.rs",
  "short_name": "memegen",
  "icons": [
    { "src": "/icon-192.png", "type": "image/png", "sizes": "192x192" },
    { "src": "/icon-512.png", "type": "image/png", "sizes": "512x512" },
    { "src": "/icon-512.png", "type": "image/png", "sizes": "512x512", "purpose": "maskable" }
  ],
  "theme_color": "#0b0c0e",
  "background_color": "#0b0c0e",
  "display": "standalone"
}"##;

fn static_asset(bytes: &'static [u8], mime: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, IMMUTABLE),
        ],
        Bytes::from_static(bytes),
    )
        .into_response()
}

/// `/SKILL.md`, `/llms.txt`, and any-case variants (`/skill.md`, `/LLMS.TXT`,
/// ...) all serve the same embedded agent doc. Everything else is a 404.
async fn docs_fallback(uri: Uri) -> Response {
    let path = uri.path();
    if path.eq_ignore_ascii_case("/skill.md") {
        return agent_doc("text/markdown; charset=utf-8");
    }
    if path.eq_ignore_ascii_case("/llms.txt") {
        return agent_doc("text/plain; charset=utf-8");
    }
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "not found" })),
    )
        .into_response()
}

fn agent_doc(mime: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        SKILL_MD,
    )
        .into_response()
}

// ---------- Front-end (gallery + builder) ----------

const PAGE_CSS: &str = r#"
@font-face{font-family:"Anton";src:url("/font/anton.ttf") format("truetype");font-display:swap}
:root{--bg:#0b0c0e;--surface:#15171c;--surface2:#1c1f26;--border:#272b33;--fg:#eceef2;--muted:#8b919e;--accent:#ff7a45;--accent-ink:#1a0d06;--radius:14px}
*{box-sizing:border-box}
html{scroll-behavior:smooth;-webkit-text-size-adjust:100%}
body{margin:0;background:var(--bg);color:var(--fg);font-family:system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;line-height:1.55;-webkit-font-smoothing:antialiased;overflow-wrap:break-word}
a{color:inherit}
img{max-width:100%}
/* --- mobile first --- */
.wrap{width:100%;max-width:72rem;margin:0 auto;padding:1.25rem 1rem 3rem}
.topbar{position:sticky;top:0;z-index:10;display:flex;align-items:center;gap:.75rem;padding:.6rem 1rem;background:color-mix(in oklab,var(--bg) 82%,transparent);backdrop-filter:blur(10px);border-bottom:1px solid var(--border)}
.brand{font-family:"Anton",Impact,sans-serif;font-size:1.25rem;letter-spacing:.02em;text-decoration:none;color:var(--fg)}
.brand .dot{color:var(--accent)}
.topbar .spacer{flex:1}
.gh{display:inline-flex;color:var(--muted);opacity:.7;padding:.25rem;transition:opacity .15s,color .15s}
.gh svg{width:24px;height:24px;display:block;fill:currentColor}
.hero{padding:1.75rem 0 .25rem;max-width:46rem}
.hero h1{font-family:"Anton",Impact,sans-serif;font-weight:400;font-size:clamp(2rem,8.5vw,4rem);line-height:1;letter-spacing:.01em;margin:0 0 .6rem;text-transform:uppercase}
.hero h1 .accent{color:var(--accent)}
.hero p{color:var(--muted);font-size:1rem;margin:0}
.toolbar{display:flex;flex-wrap:wrap;align-items:center;gap:.6rem;margin:1.25rem 0 1rem}
#search{flex:1 1 12rem;min-width:0;min-height:2.75rem;background:var(--surface);border:1px solid var(--border);color:var(--fg);border-radius:10px;padding:.6rem .9rem;font-size:1rem;outline:none}
#search:focus{border-color:var(--accent)}
.count{color:var(--muted);font-size:.85rem;white-space:nowrap}
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(min(100%,8.5rem),1fr));gap:.7rem}
.card{background:var(--surface);border:1px solid var(--border);border-radius:var(--radius);overflow:hidden;text-decoration:none;display:flex;flex-direction:column}
.thumb{width:100%;aspect-ratio:1;object-fit:cover;background:var(--surface2);display:block}
.meta{padding:.5rem .6rem;display:flex;flex-direction:column;gap:.35rem}
.tname{font-family:"Anton",Impact,sans-serif;font-size:.9rem;letter-spacing:.02em;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;text-transform:uppercase}
.tags{display:flex;gap:.35rem;flex-wrap:wrap}
.badge{font-size:.6rem;color:var(--muted);border:1px solid var(--border);border-radius:999px;padding:.05rem .45rem;letter-spacing:.03em}
.badge.gif{color:var(--accent-ink);background:var(--accent);border-color:var(--accent);font-weight:700}
.card[hidden]{display:none}
.empty{color:var(--muted);padding:2rem 0}
.builder{display:grid;grid-template-columns:minmax(0,1fr);gap:1.25rem;margin-top:1rem}
.preview-wrap{min-width:0;background:var(--surface);border:1px solid var(--border);border-radius:var(--radius);padding:.75rem}
#preview{width:100%;border-radius:8px;display:block;background:var(--surface2);min-height:120px}
.panel h1{font-family:"Anton",Impact,sans-serif;font-weight:400;font-size:1.6rem;margin:.1rem 0 .25rem;text-transform:uppercase;letter-spacing:.01em}
.panel .src{color:var(--muted);font-size:.85rem;margin:0 0 1rem}
.field{margin:0 0 .85rem}
.field label{display:block;font-size:.72rem;color:var(--muted);margin:0 0 .3rem;letter-spacing:.05em;text-transform:uppercase}
.field input[type=text]{width:100%;min-height:2.75rem;background:var(--surface);border:1px solid var(--border);color:var(--fg);border-radius:10px;padding:.6rem .8rem;font-size:1rem;outline:none}
.field input[type=text]:focus{border-color:var(--accent)}
.toggle{display:inline-flex;align-items:center;gap:.5rem;color:var(--muted);font-size:.9rem;margin:.1rem 0 1rem;min-height:2.5rem}
.urlbox{background:#0e1014;border:1px solid var(--border);border-radius:10px;padding:.7rem .85rem;margin:.3rem 0 1.1rem;overflow-x:auto;-webkit-overflow-scrolling:touch}
.urlbox code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.8rem;color:var(--accent);white-space:nowrap}
.actions{display:flex;gap:.6rem;flex-wrap:wrap}
.btn{flex:1 1 auto;cursor:pointer;font:inherit;font-weight:600;text-align:center;text-decoration:none;border-radius:10px;min-height:2.75rem;display:inline-flex;align-items:center;justify-content:center;padding:.6rem 1.1rem;border:1px solid var(--border);background:var(--surface);color:var(--fg);transition:border-color .15s,background .15s,filter .15s}
.btn.primary{background:var(--accent);color:var(--accent-ink);border-color:var(--accent)}
.back{display:inline-block;color:var(--muted);text-decoration:none;font-size:.9rem;margin:0 0 .5rem}
.back:hover{color:var(--fg)}
footer{margin-top:3rem;border-top:1px solid var(--border);padding:1.5rem 1rem 0;color:var(--muted);font-size:.85rem;max-width:72rem;margin-left:auto;margin-right:auto}
footer a{color:var(--muted);text-decoration:underline;text-underline-offset:2px}
footer a:hover{color:var(--fg)}
:focus-visible{outline:2px solid var(--accent);outline-offset:2px}
/* --- enhance for larger viewports --- */
@media(min-width:36rem){
  .wrap{padding:1.5rem 1.25rem 4rem}
  .grid{gap:1rem;grid-template-columns:repeat(auto-fill,minmax(10rem,1fr))}
  #search{flex:1 1 16rem}
  .btn{flex:0 0 auto}
  .hero p{font-size:1.1rem}
}
@media(min-width:48rem){
  .topbar{padding:.7rem 1.25rem}
  .brand{font-size:1.4rem}
  .hero{padding:2.5rem 0 .5rem}
  .builder{grid-template-columns:minmax(0,1fr) minmax(0,1fr);gap:2rem;margin-top:1.25rem}
  .preview-wrap{position:sticky;top:5rem;align-self:start;padding:1rem}
  .panel h1{font-size:2rem}
}
@media(hover:hover){
  .card{transition:border-color .15s,transform .15s}
  .card:hover{border-color:var(--accent);transform:translateY(-2px)}
  .gh:hover{opacity:1;color:var(--fg)}
  .btn:hover{border-color:var(--accent)}
  .btn.primary:hover{filter:brightness(1.07)}
}
@media(prefers-reduced-motion:reduce){*{transition:none!important;scroll-behavior:auto!important}}
"#;

const GH_ICON: &str = r#"<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82a7.65 7.65 0 0 1 2-.27c.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0 0 16 8c0-4.42-3.58-8-8-8z"></path></svg>"#;

const GALLERY_JS: &str = r#"
(function(){var q=document.getElementById('search');var cards=[].slice.call(document.querySelectorAll('.card'));var none=document.getElementById('empty');if(!q)return;q.addEventListener('input',function(){var v=q.value.trim().toLowerCase();var shown=0;for(var i=0;i<cards.length;i++){var hit=v===''||cards[i].dataset.search.indexOf(v)!==-1;cards[i].hidden=!hit;if(hit)shown++;}if(none)none.hidden=shown!==0;});})();
"#;

const BUILDER_JS: &str = r#"
(function(){var id=document.body.dataset.template;var inputs=[].slice.call(document.querySelectorAll('.cap'));var img=document.getElementById('preview');var urlEl=document.getElementById('url');var gif=document.getElementById('gif');var t;
function enc(s){return s===''?'_':s.replace(/_/g,'__').replace(/ /g,'_').replace(/\?/g,'~q').replace(/&/g,'~a').replace(/%/g,'~p').replace(/#/g,'~h').replace(/\//g,'~s').replace(/"/g,"''");}
function path(){var ext=(gif&&gif.checked)?'gif':'png';return '/images/'+id+'/'+inputs.map(function(i){return enc(i.value);}).join('/')+'.'+ext;}
function update(){var p=path();urlEl.textContent=location.origin+p;clearTimeout(t);t=setTimeout(function(){img.src=p;},400);}
inputs.forEach(function(i){i.addEventListener('input',update);});if(gif)gif.addEventListener('change',update);update();
function flash(b,m){b.textContent=m;setTimeout(function(){b.textContent=b.dataset.label;},1300);}
var link=document.getElementById('copy-link');link.dataset.label=link.textContent;
link.addEventListener('click',function(){navigator.clipboard.writeText(location.origin+path()).then(function(){flash(link,'Copied!');},function(){flash(link,'Copy failed');});});
var ci=document.getElementById('copy-img');ci.dataset.label=ci.textContent;
ci.addEventListener('click',function(){fetch(path()).then(function(r){if(!r.ok){flash(ci,r.status===429?'Rate limited':'Error');return null;}return r.blob();}).then(function(b){if(!b)return;return navigator.clipboard.write([new ClipboardItem(Object.fromEntries([[b.type,b]]))]).then(function(){flash(ci,'Copied!');});}).catch(function(){flash(ci,'Copy failed');});});})();
"#;

fn display_name(t: &Template) -> &str {
    if t.name.is_empty() { &t.id } else { &t.name }
}

fn search_blob(t: &Template) -> String {
    format!("{} {} {}", t.id, t.name, t.keywords.join(" ")).to_lowercase()
}

fn lines_label(n: usize) -> String {
    if n == 1 {
        "1 line".into()
    } else {
        format!("{n} lines")
    }
}

fn topbar() -> Markup {
    html! {
        header class="topbar" {
            a class="brand" href="/" { "memegen" span class="dot" { ".rs" } }
            span class="spacer" {}
            a class="gh" href="https://github.com/tenequm/memegen-rs"
                title="GitHub repository" target="_blank" rel="noreferrer" {
                (PreEscaped(GH_ICON))
            }
        }
    }
}

// Social-share metadata first (Slack reads only the first 32 KB of <head>, so
// the OG block precedes the stylesheet). One 1200x630 JPEG card + the OG tags +
// twitter:card=summary_large_image is the cross-platform lowest common
// denominator (Telegram, Slack, Discord, LinkedIn, Reddit, X, Facebook).
fn page_head(title: &str, desc: &str, path: &str, og_image: &str) -> Markup {
    let canonical = format!("{SITE}{path}");
    html! {
        head {
            meta charset="utf-8";
            meta name="viewport" content="width=device-width, initial-scale=1";
            title { (title) }
            meta name="description" content=(desc);
            link rel="canonical" href=(canonical);

            meta property="og:type" content="website";
            meta property="og:site_name" content="memegen.rs";
            meta property="og:title" content=(title);
            meta property="og:description" content=(desc);
            meta property="og:url" content=(canonical);
            meta property="og:image" content=(og_image);
            meta property="og:image:type" content="image/jpeg";
            meta property="og:image:width" content="1200";
            meta property="og:image:height" content="630";
            meta property="og:image:alt" content=(title);
            meta name="twitter:card" content="summary_large_image";
            meta name="twitter:image" content=(og_image);

            link rel="icon" href="/favicon.ico" sizes="32x32";
            link rel="icon" href="/favicon.svg" type="image/svg+xml" sizes="any";
            link rel="apple-touch-icon" href="/apple-touch-icon.png";
            link rel="manifest" href="/manifest.webmanifest";
            meta name="theme-color" content="#0b0c0e";

            style { (PreEscaped(PAGE_CSS)) }
        }
    }
}

/// A template's own example meme as a 1200x630 JPEG social card.
fn og_image_for(t: &Template) -> String {
    let jpg = t.example_path().replace(".png", ".jpg");
    format!("{SITE}{jpg}?width=1200&height=630")
}

fn page_footer() -> Markup {
    html! {
        footer {
            "Built with Rust, axum & maud - "
            a href="https://github.com/tenequm/memegen-rs" { "source on GitHub" }
            ". A minimal reimplementation of "
            a href="https://github.com/jacebrowning/memegen" { "memegen" }
            ". Template images belong to their respective owners."
        }
    }
}

// The gallery is a pure function of the immutable registry, so render it once
// and serve the cached string on every hit (no per-request HTML build or stats).
async fn gallery(State(reg): State<AppState>) -> Html<&'static str> {
    static CACHE: OnceLock<String> = OnceLock::new();
    Html(
        CACHE
            .get_or_init(|| gallery_markup(&reg).into_string())
            .as_str(),
    )
}

fn gallery_markup(reg: &Registry) -> Markup {
    let count = reg.len();
    html! {
        (DOCTYPE)
        html lang="en" {
            (page_head(
                "memegen.rs - meme generator",
                "A tiny, stateless meme generator in pure Rust. Pick a template, type your caption, copy the link.",
                "/",
                BRAND_OG,
            ))
            body {
                (topbar())
                main class="wrap" {
                    section class="hero" {
                        h1 { "Every meme is " span class="accent" { "just a URL" } "." }
                        p { "A tiny, stateless meme generator in pure Rust. Pick a template, type your caption, copy the link or the image." }
                    }
                    div class="toolbar" {
                        input #search type="search" placeholder="Search templates by name or keyword..."
                            autocomplete="off" aria-label="Search templates";
                        span class="count" { (count) " templates" }
                    }
                    p #empty class="empty" hidden { "No templates match that search." }
                    div class="grid" {
                        @for t in reg.all() {
                            @let is_gif = t.is_gif;
                            a class="card" href={ "/edit/" (t.id) } data-search=(search_blob(t)) {
                                img class="thumb" src={ "/thumbs/" (t.id) } loading="lazy"
                                    decoding="async" alt=(display_name(t));
                                div class="meta" {
                                    span class="tname" { (display_name(t)) }
                                    div class="tags" {
                                        @if is_gif { span class="badge gif" { "GIF" } }
                                        span class="badge" { (lines_label(t.lines())) }
                                    }
                                }
                            }
                        }
                    }
                }
                (page_footer())
                script { (PreEscaped(GALLERY_JS)) }
            }
        }
    }
}

async fn builder(State(reg): State<AppState>, Path(id): Path<String>) -> Result<Markup, AppError> {
    let t = reg
        .get(&id)
        .ok_or_else(|| AppError::NotFound(format!("template not found: {id}")))?;
    let n = t.lines().max(1);
    let is_gif = t.is_gif;
    let name = display_name(t);
    let initial = t.example_path();
    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            (page_head(
                &format!("{name} - memegen.rs"),
                &format!("Caption the {name} meme template and copy the link or the image."),
                &format!("/edit/{}", t.id),
                &og_image_for(t),
            ))
            body data-template=(t.id) {
                (topbar())
                main class="wrap" {
                    a class="back" href="/" { "Back to all templates" }
                    div class="builder" {
                        div class="preview-wrap" {
                            img #preview src=(initial) alt={ (name) " preview" };
                        }
                        div class="panel" {
                            h1 { (name) }
                            @if let Some(src) = &t.source {
                                p class="src" {
                                    a href=(src) target="_blank" rel="noreferrer" { "source" }
                                }
                            }
                            @for i in 0..n {
                                @let val = t.example.get(i).map(String::as_str).unwrap_or("");
                                div class="field" {
                                    label for=(format!("cap{i}")) { "Caption " (i + 1) }
                                    input #(format!("cap{i}")) class="cap" type="text"
                                        value=(val) placeholder=(format!("Caption {}", i + 1));
                                }
                            }
                            @if is_gif {
                                label class="toggle" {
                                    input #gif type="checkbox"; "Animated GIF output"
                                }
                            }
                            div class="urlbox" { code #url { (initial) } }
                            div class="actions" {
                                button #copy-link class="btn primary" type="button" { "Copy link" }
                                button #copy-img class="btn" type="button" { "Copy image" }
                                a class="btn" href="/docs" { "More options" }
                            }
                        }
                    }
                }
                (page_footer())
                script { (PreEscaped(BUILDER_JS)) }
            }
        }
    };
    Ok(markup)
}

// ---------- Helpers ----------

fn split_ext(s: &str) -> (&str, &str) {
    s.rsplit_once('.').unwrap_or((s, "png"))
}

fn mime_for(p: &std::path::Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "application/octet-stream",
    }
}

fn cached_bytes(bytes: Vec<u8>, mime: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, IMMUTABLE),
        ],
        bytes,
    )
        .into_response()
}

async fn fetch(url: &str) -> Result<Vec<u8>, AppError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| AppError::Unprocessable(format!("could not fetch background: {e}")))?;
    if !resp.status().is_success() {
        return Err(AppError::Unprocessable(format!(
            "background URL returned {}",
            resp.status()
        )));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| AppError::Unprocessable(e.to_string()))
}

// ---------- Errors ----------

enum AppError {
    NotFound(String),
    BadRequest(String),
    Unprocessable(String),
    Internal(String),
}

impl From<RenderError> for AppError {
    fn from(e: RenderError) -> Self {
        match e {
            RenderError::NoBackground => {
                AppError::Unprocessable("no static background for this template".into())
            }
            RenderError::Unsupported(ext) => {
                AppError::Unprocessable(format!("unsupported extension: {ext}"))
            }
            RenderError::Decode(m) => AppError::Unprocessable(format!("cannot decode image: {m}")),
            RenderError::Encode(m) => AppError::Internal(format!("cannot encode image: {m}")),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            AppError::Unprocessable(m) => (StatusCode::UNPROCESSABLE_ENTITY, m),
            AppError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

// ---------- Agent-facing docs (llms.txt / SKILL.md) ----------

/// Served at `/SKILL.md`, `/llms.txt`, and any-case variants. `assets/SKILL.md`
/// is the single source of truth (root `SKILL.md` is a symlink to it; the
/// release pipeline publishes the same file to ClawHub).
const SKILL_MD: &str = include_str!("../assets/SKILL.md");

#[derive(OpenApi)]
#[openapi(
    paths(list_templates, get_template, render_text, render_blank, render_custom),
    components(schemas(TemplateDto, ExampleDto))
)]
struct ApiDoc;
