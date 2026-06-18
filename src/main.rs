mod render;
mod template;

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use maud::{DOCTYPE, Markup, PreEscaped, html};
use serde::{Deserialize, Serialize};
use tower_governor::GovernorLayer;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::GlobalKeyExtractor;
use utoipa::{OpenApi, ToSchema};
use utoipa_scalar::{Scalar, Servable};

use render::{RenderError, Spec};
use template::{Registry, Template, decode};

type AppState = Arc<Registry>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = std::env::var("MEMEGEN_TEMPLATES_DIR").unwrap_or_else(|_| "templates".into());
    let registry = Arc::new(Registry::load(&PathBuf::from(&dir))?);
    println!("loaded {} templates from {dir}", registry.len());

    // Global hard cap of 5 requests/second (bucket of 5, one slot refilled every
    // 200ms). Behind a reverse proxy the peer IP is the proxy, so a global key is
    // both the simplest and the only meaningful choice here.
    let governor_conf = GovernorConfigBuilder::default()
        .burst_size(5)
        .per_millisecond(200)
        .key_extractor(GlobalKeyExtractor)
        .finish()
        .expect("valid rate-limit config");

    let app = Router::new()
        .route("/openapi.json", get(openapi))
        .route("/templates", get(list_templates))
        .route("/templates/{id}", get(get_template))
        .route("/images/custom/{*text}", get(render_custom))
        .route("/images/{id}/{*text}", get(render_text))
        .route("/images/{filename}", get(render_blank))
        .route("/", get(homepage))
        .with_state(registry)
        // Scalar API docs (rendered from the OpenAPI spec).
        .merge(Scalar::with_url("/docs", ApiDoc::openapi()))
        .layer(GovernorLayer::new(governor_conf));

    let port = std::env::var("PORT").unwrap_or_else(|_| "5005".into());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    println!("listening on http://0.0.0.0:{port}");
    axum::serve(listener, app).await?;
    Ok(())
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

// ---------- Handlers ----------

async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

const HOMEPAGE_CSS: &str = r#"
:root{--bg:#0d1117;--card:#161b22;--fg:#e6edf3;--muted:#8b949e;--accent:#f0883e;--border:#30363d}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);font-family:system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;line-height:1.6}
.wrap{max-width:52rem;margin:0 auto;padding:4rem 1.25rem}
h1{font-size:3rem;margin:0;letter-spacing:-.03em;background:linear-gradient(90deg,#f0883e,#e3b341);-webkit-background-clip:text;background-clip:text;color:transparent}
.tagline{font-size:1.2rem;color:var(--muted);margin:.5rem 0 2.5rem}
.examples{display:grid;grid-template-columns:1fr 1fr;gap:1rem;margin:0 0 2.5rem}
@media(max-width:40rem){.examples{grid-template-columns:1fr}}
figure{margin:0;background:var(--card);border:1px solid var(--border);border-radius:12px;padding:.75rem}
figure img{width:100%;border-radius:8px;display:block}
figcaption{margin-top:.5rem;font-size:.7rem;color:var(--muted);word-break:break-all}
h2{font-size:1.4rem;margin:2rem 0 .5rem}
pre{background:var(--card);border:1px solid var(--border);border-radius:8px;padding:1rem;overflow:auto}
code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.9em}
pre code{color:var(--accent)}
:not(pre)>code{background:var(--card);border:1px solid var(--border);padding:.1em .4em;border-radius:5px;color:var(--accent)}
.muted{color:var(--muted);font-size:.9rem}
.links{display:flex;gap:.75rem;flex-wrap:wrap;margin:2.5rem 0 0}
.btn{text-decoration:none;color:var(--fg);background:var(--card);border:1px solid var(--border);padding:.6rem 1.2rem;border-radius:8px;font-weight:600;transition:border-color .15s}
.btn:hover{border-color:var(--accent)}
.btn.primary{background:var(--accent);color:#0d1117;border-color:var(--accent)}
footer{margin-top:3rem;border-top:1px solid var(--border);padding-top:1.5rem}
"#;

async fn homepage(State(reg): State<AppState>) -> Markup {
    let count = reg.len();
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "memegen.rs - meme generator API" }
                meta name="description" content="A tiny, stateless meme generator API in pure Rust. Every meme is just a URL.";
                style { (PreEscaped(HOMEPAGE_CSS)) }
            }
            body {
                main class="wrap" {
                    header {
                        h1 { "memegen.rs" }
                        p class="tagline" {
                            "A tiny, stateless meme generator API in pure Rust. Every meme is just a URL."
                        }
                    }
                    section class="examples" {
                        figure {
                            img src="/images/drake/running_a_whole_CMS/a_URL_that_is_the_meme.png" alt="example meme" loading="lazy";
                            figcaption { code { "/images/drake/running_a_whole_CMS/a_URL_that_is_the_meme.png" } }
                        }
                        figure {
                            img src="/images/fry/not_sure_if_homepage/or_just_api_docs.png" alt="example meme" loading="lazy";
                            figcaption { code { "/images/fry/not_sure_if_homepage/or_just_api_docs.png" } }
                        }
                    }
                    section {
                        h2 { "How it works" }
                        p { "Build a URL, get a PNG. Underscores become spaces, slashes separate caption lines." }
                        pre { code { "GET /images/{template}/{top}/{bottom}.png" } }
                        p class="muted" {
                            (count) " templates loaded. Caption any image on the web with "
                            code { "/images/custom/top/bottom.png?background=<url>" } "."
                        }
                    }
                    nav class="links" {
                        a class="btn primary" href="/docs" { "API docs" }
                        a class="btn" href="/openapi.json" { "OpenAPI spec" }
                        a class="btn" href="https://github.com/tenequm/memegen-rs" { "GitHub" }
                    }
                    footer {
                        p class="muted" {
                            "Built with Rust, axum & maud. MIT licensed. Template images belong to their respective owners."
                        }
                    }
                }
            }
        }
    }
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
    Ok(image_response(bytes, mime))
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
    Ok(image_response(bytes, mime))
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
    Ok(image_response(out, mime))
}

// ---------- Helpers ----------

fn split_ext(s: &str) -> (&str, &str) {
    s.rsplit_once('.').unwrap_or((s, "png"))
}

fn image_response(bytes: Vec<u8>, mime: &'static str) -> Response {
    ([(header::CONTENT_TYPE, mime)], bytes).into_response()
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

#[derive(OpenApi)]
#[openapi(
    paths(list_templates, get_template, render_text, render_blank, render_custom),
    components(schemas(TemplateDto, ExampleDto))
)]
struct ApiDoc;
