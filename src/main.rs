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
use serde::{Deserialize, Serialize};
use utoipa::{OpenApi, ToSchema};

use render::{RenderError, Spec};
use template::{Registry, Template, decode};

type AppState = Arc<Registry>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = std::env::var("MEMEGEN_TEMPLATES_DIR").unwrap_or_else(|_| "templates".into());
    let registry = Arc::new(Registry::load(&PathBuf::from(&dir))?);
    println!("loaded {} templates from {dir}", registry.len());

    let app = Router::new()
        .route("/", get(index))
        .route("/openapi.json", get(openapi))
        .route("/templates", get(list_templates))
        .route("/templates/{id}", get(get_template))
        .route("/images/custom/{*text}", get(render_custom))
        .route("/images/{id}/{*text}", get(render_text))
        .route("/images/{filename}", get(render_blank))
        .with_state(registry);

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

#[derive(Serialize)]
struct Index {
    name: &'static str,
    templates: usize,
    openapi: &'static str,
}

async fn index(State(reg): State<AppState>) -> Json<Index> {
    Json(Index {
        name: "memegen-rs",
        templates: reg.len(),
        openapi: "/openapi.json",
    })
}

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
