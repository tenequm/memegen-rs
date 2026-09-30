//! Bounded local cache for rendered images, layered onto the image routes as
//! middleware. Off unless `MEMEGEN_CACHE_DIR` is set.

use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::Context;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use foyer::{
    BlockEngineConfig, Code, DeviceBuilder, FsDeviceBuilder, HybridCache, HybridCacheBuilder,
    RecoverMode, Source,
};

const DEFAULT_MAX_BYTES: usize = 10 << 30;
const DEFAULT_MEMORY_BYTES: usize = 16 << 20;
const MIN_MAX_BYTES: usize = 1 << 20;

/// The disk tier is a fixed set of equally sized block files, and a block is
/// both the eviction unit and the largest entry it can hold. Eight blocks is
/// the fewest that still leaves most of the budget usable while one is being
/// reclaimed; 64 MiB caps the open files at 160 for the default 10 GiB.
const MIN_BLOCKS: usize = 8;
const MAX_BLOCK_BYTES: usize = 64 << 20;

/// What an entry holds in memory beyond its key and encoded size: the record,
/// the header map's tables, allocator slack. Left out, a flood of tiny renders
/// would keep several times `MEMEGEN_CACHE_MEMORY_BYTES`.
const ENTRY_OVERHEAD: usize = 1 << 10;

const X_CACHE: HeaderName = HeaderName::from_static("x-memegen-cache");

#[derive(Debug, PartialEq)]
struct Config {
    dir: PathBuf,
    max_bytes: usize,
    memory_bytes: usize,
}

impl Config {
    fn from_vars(var: impl Fn(&str) -> Option<String>) -> anyhow::Result<Option<Self>> {
        let Some(dir) = var("MEMEGEN_CACHE_DIR").filter(|dir| !dir.is_empty()) else {
            return Ok(None);
        };
        let bytes = |name: &str, default: usize| match var(name) {
            Some(raw) => raw
                .parse()
                .map_err(|e| anyhow::anyhow!("{name}={raw:?} is not a byte count: {e}")),
            None => Ok(default),
        };
        let max_bytes = bytes("MEMEGEN_CACHE_MAX_BYTES", DEFAULT_MAX_BYTES)?;
        anyhow::ensure!(
            max_bytes >= MIN_MAX_BYTES,
            "MEMEGEN_CACHE_MAX_BYTES must be at least {MIN_MAX_BYTES}"
        );
        Ok(Some(Self {
            dir: dir.into(),
            max_bytes,
            memory_bytes: bytes("MEMEGEN_CACHE_MEMORY_BYTES", DEFAULT_MEMORY_BYTES)?,
        }))
    }
}

#[derive(Clone)]
pub(crate) struct Cache(HybridCache<String, Rendered>);

impl Cache {
    pub(crate) async fn from_env() -> anyhow::Result<Option<Self>> {
        let Some(config) = Config::from_vars(|name| std::env::var(name).ok())? else {
            return Ok(None);
        };
        let cache = Self::open(&config)
            .await
            .with_context(|| format!("cannot cache in {}", config.dir.display()))?;
        Ok(Some(cache))
    }

    async fn open(config: &Config) -> anyhow::Result<Self> {
        // foyer unwraps its own attempt, so a bad path would be a panic.
        tokio::fs::create_dir_all(&config.dir).await?;
        let device = FsDeviceBuilder::new(&config.dir)
            .with_capacity(config.max_bytes)
            .build()?;
        let block_bytes = (config.max_bytes / MIN_BLOCKS).min(MAX_BLOCK_BYTES);
        let cache = HybridCacheBuilder::new()
            // A render also depends on the templates and MEMEGEN_WATERMARK,
            // which are not in the key, so nothing may outlive the process:
            // no flush on the way out, no recovery on the way in.
            .with_flush_on_close(false)
            .memory(config.memory_bytes)
            // One shard makes the memory bound exact: a shard may keep a single
            // entry larger than its share, so eight shards could hold eight.
            .with_shards(1)
            .with_weighter(|key: &String, value: &Rendered| {
                key.len() + value.estimated_size() + ENTRY_OVERHEAD
            })
            .storage()
            .with_engine_config(BlockEngineConfig::new(device).with_block_size(block_bytes))
            .with_recover_mode(RecoverMode::None)
            .build()
            .await?;
        println!(
            "caching renders in {} (max {} bytes on disk, {} in memory)",
            config.dir.display(),
            config.max_bytes,
            config.memory_bytes
        );
        Ok(Self(cache))
    }
}

/// Serves a cached 200 when there is one; otherwise runs the handler once per
/// key no matter how many identical requests are waiting, and keeps its 200.
pub(crate) async fn serve(State(cache): State<Cache>, req: Request, next: Next) -> Response {
    // `get` routes also answer HEAD; those run the handler as they always did.
    if req.method() != Method::GET {
        return next.run(req).await;
    }
    let key = req
        .uri()
        .path_and_query()
        .map_or_else(|| req.uri().path(), |pq| pq.as_str())
        .to_owned();
    let fetched = cache
        .0
        .get_or_fetch(&key, || async move {
            let (parts, body) = next.run(req).await.into_parts();
            let body = to_bytes(body, usize::MAX).await?;
            let rendered = Rendered {
                headers: parts.headers,
                // An encoder's buffer keeps its spare capacity alive, which the
                // memory bound cannot see; an exact copy can be accounted for.
                body: Bytes::copy_from_slice(&body),
            };
            if parts.status == StatusCode::OK {
                Ok(rendered)
            } else {
                Err(anyhow::Error::new(Uncached(parts.status, rendered)))
            }
        })
        .await;
    match fetched {
        Ok(entry) => {
            let state = match entry.source() {
                Source::Outer => "miss",
                Source::Memory | Source::Disk => "hit",
            };
            let mut response = entry.value().clone().into_response();
            response
                .headers_mut()
                .insert(X_CACHE, HeaderValue::from_static(state));
            response
        }
        Err(e) => match e.downcast_ref::<Uncached>() {
            Some(Uncached(status, rendered)) => (*status, rendered.clone()).into_response(),
            None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
    }
}

#[derive(Clone, Debug)]
struct Rendered {
    headers: HeaderMap,
    body: Bytes,
}

impl IntoResponse for Rendered {
    fn into_response(self) -> Response {
        let mut response = Response::new(Body::from(self.body));
        *response.headers_mut() = self.headers;
        response
    }
}

impl Code for Rendered {
    fn encode(&self, writer: &mut impl Write) -> foyer::Result<()> {
        self.headers.len().encode(writer)?;
        for (name, value) in &self.headers {
            name.as_str().to_owned().encode(writer)?;
            value.as_bytes().to_vec().encode(writer)?;
        }
        self.body.encode(writer)
    }

    fn decode(reader: &mut impl Read) -> foyer::Result<Self> {
        let invalid = |what| foyer::Error::new(foyer::ErrorKind::Parse, what);
        let mut headers = HeaderMap::new();
        for _ in 0..usize::decode(reader)? {
            let name = HeaderName::try_from(String::decode(reader)?)
                .map_err(|_| invalid("cached header name"))?;
            let value = HeaderValue::try_from(Vec::decode(reader)?)
                .map_err(|_| invalid("cached header value"))?;
            headers.append(name, value);
        }
        Ok(Self {
            headers,
            body: Bytes::decode(reader)?,
        })
    }

    fn estimated_size(&self) -> usize {
        let len = size_of::<usize>();
        let headers: usize = self
            .headers
            .iter()
            .map(|(name, value)| 2 * len + name.as_str().len() + value.len())
            .sum();
        len + headers + len + self.body.len()
    }
}

/// A non-200 handed to every request waiting on the same key, never stored.
#[derive(Debug)]
struct Uncached(StatusCode, Rendered);

impl fmt::Display for Uncached {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "handler answered {}", self.0)
    }
}

impl std::error::Error for Uncached {}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::Router;
    use axum::http::header::{CONTENT_TYPE, DATE};
    use axum::middleware::from_fn_with_state;
    use axum::routing::get;

    use super::*;

    const BODY_BYTES: usize = 40 << 10;

    async fn open(dir: &Path, max_bytes: usize) -> Cache {
        let config = Config {
            dir: dir.into(),
            max_bytes,
            memory_bytes: 0,
        };
        Cache::open(&config).await.expect("open cache")
    }

    /// Moves everything out of the memory tier, so the next read is from disk.
    async fn settle(cache: &Cache) {
        cache.0.memory().flush().await;
        cache.0.storage().wait().await;
    }

    async fn serve_on_a_spare_port(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// Stands in for the render handlers: counts its calls, answers a body
    /// derived from the path, and is slow or a 404 on request.
    fn counting_app(cache: Cache, renders: Arc<AtomicUsize>) -> Router {
        let render = move |axum::extract::Path(key): axum::extract::Path<String>| async move {
            renders.fetch_add(1, Ordering::SeqCst);
            if key.starts_with("slow") {
                tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(200)))
                    .await
                    .unwrap();
            }
            if key.starts_with("missing") {
                return (StatusCode::NOT_FOUND, "no such template").into_response();
            }
            let body: Vec<u8> = key.bytes().cycle().take(BODY_BYTES).collect();
            ([(CONTENT_TYPE, "image/png")], body).into_response()
        };
        Router::new()
            .route("/images/{*key}", get(render))
            .route_layer(from_fn_with_state(cache, serve))
    }

    fn real_app(cache: Option<Cache>) -> Router {
        let templates = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
        let registry = crate::template::Registry::load(&templates).expect("load templates");
        crate::app(Arc::new(registry), cache)
    }

    /// Status, headers minus the per-response ones, cache verdict, body.
    async fn fetch(url: &str) -> (StatusCode, HeaderMap, Option<String>, Bytes) {
        let response = reqwest::get(url).await.expect("request");
        let status = response.status();
        let mut headers = response.headers().clone();
        headers.remove(DATE);
        let verdict = headers
            .remove(X_CACHE)
            .map(|v| v.to_str().unwrap().to_owned());
        (status, headers, verdict, response.bytes().await.unwrap())
    }

    fn vars(pairs: &'static [(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn unset_dir_means_no_cache() {
        assert_eq!(Config::from_vars(vars(&[])).unwrap(), None);
        assert_eq!(
            Config::from_vars(vars(&[("MEMEGEN_CACHE_DIR", "")])).unwrap(),
            None
        );
        let sized_but_off = vars(&[("MEMEGEN_CACHE_MAX_BYTES", "4096")]);
        assert_eq!(Config::from_vars(sized_but_off).unwrap(), None);
    }

    #[test]
    fn dir_alone_gets_the_default_bounds() {
        let config = Config::from_vars(vars(&[("MEMEGEN_CACHE_DIR", "/cache")]));
        assert_eq!(
            config.unwrap(),
            Some(Config {
                dir: "/cache".into(),
                max_bytes: 10_737_418_240,
                memory_bytes: 16 << 20,
            })
        );
    }

    #[test]
    fn unusable_bounds_fail_startup() {
        for (name, value) in [
            ("MEMEGEN_CACHE_MAX_BYTES", "10G"),
            ("MEMEGEN_CACHE_MAX_BYTES", "4096"),
            ("MEMEGEN_CACHE_MEMORY_BYTES", "-1"),
        ] {
            let var = |asked: &str| match asked {
                "MEMEGEN_CACHE_DIR" => Some("/cache".to_owned()),
                asked if asked == name => Some(value.to_owned()),
                _ => None,
            };
            assert!(Config::from_vars(var).is_err(), "{name}={value}");
        }
    }

    #[tokio::test]
    async fn repeat_request_is_a_hit_with_identical_bytes_and_headers() {
        let dir = tempfile::tempdir().unwrap();
        let cache = open(dir.path(), 64 << 20).await;
        let cached = serve_on_a_spare_port(real_app(Some(cache.clone()))).await;
        let uncached = serve_on_a_spare_port(real_app(None)).await;
        let path = "/images/drake-hotline-bling/top/bottom.png?width=300";

        let (status, headers, verdict, body) = fetch(&format!("{uncached}{path}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[CONTENT_TYPE], "image/png");
        assert_eq!(verdict, None, "no cache, no cache header");

        let miss = fetch(&format!("{cached}{path}")).await;
        let from_memory = fetch(&format!("{cached}{path}")).await;
        settle(&cache).await;
        let from_disk = fetch(&format!("{cached}{path}")).await;
        for (got, want) in [(miss, "miss"), (from_memory, "hit"), (from_disk, "hit")] {
            assert_eq!(got.0, status);
            assert_eq!(got.1, headers);
            assert_eq!(got.2.as_deref(), Some(want));
            assert_eq!(got.3, body);
        }

        let (_, _, verdict, _) = fetch(&format!("{cached}{path}&color=red")).await;
        assert_eq!(verdict.as_deref(), Some("miss"), "the query is in the key");
    }

    #[tokio::test]
    async fn custom_backgrounds_are_never_cached() {
        let fetches = Arc::new(AtomicUsize::new(0));
        let counter = fetches.clone();
        let background = Router::new().route(
            "/bg.jpg",
            get(move || async move {
                counter.fetch_add(1, Ordering::SeqCst);
                let file = concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/templates/drake-hotline-bling/default.jpg"
                );
                tokio::fs::read(file).await.unwrap()
            }),
        );
        let background = serve_on_a_spare_port(background).await;
        let dir = tempfile::tempdir().unwrap();
        let cache = open(dir.path(), 64 << 20).await;
        let app = serve_on_a_spare_port(real_app(Some(cache))).await;
        let url = format!("{app}/images/custom/top/bottom.png?background={background}/bg.jpg");

        for _ in 0..2 {
            let (status, _, verdict, _) = fetch(&url).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(verdict, None);
        }
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn concurrent_identical_misses_render_once() {
        let dir = tempfile::tempdir().unwrap();
        let renders = Arc::new(AtomicUsize::new(0));
        let app = counting_app(open(dir.path(), 1 << 20).await, renders.clone());
        let url = format!("{}/images/slow.png", serve_on_a_spare_port(app).await);

        let requests: Vec<_> = (0..10)
            .map(|_| {
                let url = url.clone();
                tokio::spawn(async move { fetch(&url).await })
            })
            .collect();
        for request in requests {
            let (status, _, _, body) = request.await.unwrap();
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body.len(), BODY_BYTES);
        }
        assert_eq!(renders.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn errors_and_head_requests_are_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let renders = Arc::new(AtomicUsize::new(0));
        let app = counting_app(open(dir.path(), 1 << 20).await, renders.clone());
        let base = serve_on_a_spare_port(app).await;

        for _ in 0..2 {
            let (status, _, verdict, body) = fetch(&format!("{base}/images/missing.png")).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(verdict, None);
            assert_eq!(body, "no such template");
        }
        assert_eq!(renders.load(Ordering::SeqCst), 2);

        let url = format!("{base}/images/a.png");
        let head = reqwest::Client::new().head(&url).send().await.unwrap();
        assert_eq!(head.status(), StatusCode::OK);
        assert!(!head.headers().contains_key(X_CACHE));
        let (_, _, verdict, body) = fetch(&url).await;
        assert_eq!(verdict.as_deref(), Some("miss"));
        assert_eq!(body.len(), BODY_BYTES);
    }

    #[tokio::test]
    async fn eviction_holds_the_byte_bound() {
        const MAX_BYTES: usize = 1 << 20;
        let dir = tempfile::tempdir().unwrap();
        let cache = open(dir.path(), MAX_BYTES).await;
        let renders = Arc::new(AtomicUsize::new(0));
        let base = serve_on_a_spare_port(counting_app(cache.clone(), renders.clone())).await;

        // Four times the cap, written through to disk one entry at a time.
        let entries = 4 * MAX_BYTES / BODY_BYTES;
        let mut newest = Bytes::new();
        for n in 0..entries {
            let (status, _, _, body) = fetch(&format!("{base}/images/{n}.png")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body.len(), BODY_BYTES);
            newest = body;
            settle(&cache).await;
        }
        assert_eq!(renders.load(Ordering::SeqCst), entries);

        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap())
            .collect();
        assert_eq!(files.len(), MIN_BLOCKS);
        let apparent: u64 = files.iter().map(std::fs::Metadata::len).sum();
        assert!(apparent <= MAX_BYTES as u64, "{apparent} bytes on disk");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let allocated: u64 = files.iter().map(|file| file.blocks() * 512).sum();
            assert!(allocated <= MAX_BYTES as u64, "{allocated} bytes allocated");
        }

        let last = entries - 1;
        let (_, _, verdict, body) = fetch(&format!("{base}/images/{last}.png")).await;
        assert_eq!(verdict.as_deref(), Some("hit"), "the newest entry survives");
        assert_eq!(body, newest);
        let (status, _, verdict, _) = fetch(&format!("{base}/images/0.png")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(verdict.as_deref(), Some("miss"), "the oldest was evicted");
    }

    #[tokio::test]
    async fn damaged_entries_are_misses() {
        let dir = tempfile::tempdir().unwrap();
        let cache = open(dir.path(), 1 << 20).await;
        let renders = Arc::new(AtomicUsize::new(0));
        let base = serve_on_a_spare_port(counting_app(cache.clone(), renders.clone())).await;
        let url = format!("{base}/images/a.png");
        let blocks = || std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap());

        let (_, headers, _, body) = fetch(&url).await;
        settle(&cache).await;
        for block in blocks() {
            let len = block.metadata().unwrap().len() as usize;
            std::fs::write(block.path(), vec![0xAA; len]).unwrap();
        }
        let corrupt = fetch(&url).await;

        settle(&cache).await;
        for block in blocks() {
            std::fs::File::options()
                .write(true)
                .open(block.path())
                .unwrap()
                .set_len(0)
                .unwrap();
        }
        let truncated = fetch(&url).await;

        for got in [corrupt, truncated] {
            assert_eq!(got.0, StatusCode::OK);
            assert_eq!(got.1, headers);
            assert_eq!(got.2.as_deref(), Some("miss"));
            assert_eq!(got.3, body);
        }
        assert_eq!(renders.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_directory_with_content_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lost+found"), b"not ours").unwrap();
        let renders = Arc::new(AtomicUsize::new(0));

        for run in 1..=2 {
            let cache = open(dir.path(), 1 << 20).await;
            let app = counting_app(cache.clone(), renders.clone());
            let url = format!("{}/images/a.png", serve_on_a_spare_port(app).await);
            let (status, _, verdict, _) = fetch(&url).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(verdict.as_deref(), Some("miss"), "run {run}");
            settle(&cache).await;
            let (_, _, verdict, _) = fetch(&url).await;
            assert_eq!(verdict.as_deref(), Some("hit"), "run {run}");
            cache.0.close().await.unwrap();
        }
        assert_eq!(renders.load(Ordering::SeqCst), 2);
    }
}
