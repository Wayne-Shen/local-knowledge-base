use crate::{
    Progress, Runtime,
    folder_picker::AuthorizedFolder,
    markdown, models, resolve_archive, search,
    store::{ArchivedDocument, Counts, ParseError, Store},
    worker,
};
use anyhow::Result;
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::TcpListener,
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
};
use tokio_util::io::ReaderStream;

const INDEX: &str = include_str!("../web/index.html");
const CSS: &str = include_str!("../web/style.css");
const JAVASCRIPT: &str = include_str!("../web/app.js");

pub async fn run(runtime: Arc<Runtime>, listener: TcpListener) -> Result<()> {
    let app = Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route(
            "/style.css",
            get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], CSS) }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(
                        header::CONTENT_TYPE,
                        "application/javascript; charset=utf-8",
                    )],
                    JAVASCRIPT,
                )
            }),
        )
        .route("/api/status", get(status))
        .route("/api/archive", get(list_archive))
        .route("/api/open", post(open_original))
        .route("/api/theme", post(change_theme))
        .route("/api/folders", get(list_folders).post(change_folder))
        .route("/api/folders/pick", post(pick_folder))
        .route("/api/search", post(search_passages))
        .route("/api/ask", post(ask))
        .route("/api/scan", post(scan))
        .route("/api/retry", post(retry_failed))
        .route("/api/reindex", post(reindex))
        .route("/original/{hash}", get(original))
        .route("/preview/{hash}/image", get(image_preview))
        .route("/preview/{hash}/document", get(document_preview))
        .route("/preview/{hash}/{page}", get(pdf_preview))
        .route("/preview/{hash}/{page}/tables", get(pdf_tables))
        .layer(DefaultBodyLimit::max(8192))
        .layer(middleware::from_fn_with_state(runtime.clone(), local_only))
        .with_state(runtime.clone());
    let listener = tokio::net::TcpListener::from_std(listener)?;
    tracing::info!(
        "Knowledge base listening on http://{}",
        listener.local_addr()?
    );
    let shutdown_runtime = runtime.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown().await;
            shutdown_runtime.stopping.store(true, Ordering::Relaxed);
            #[cfg(target_os = "macos")]
            if shutdown_runtime.folder_access.picker_available {
                crate::folder_picker::cancel();
            }
            crate::worker::stop();
        })
        .await?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    if let Ok(mut terminate) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        return;
    }
    let _ = tokio::signal::ctrl_c().await;
}

async fn local_only(State(runtime): State<Arc<Runtime>>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let port = runtime.config().port;
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if host != format!("127.0.0.1:{port}") && host != format!("localhost:{port}") {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "Use the loopback knowledge base address",
        )
        .into_response();
    }
    if let Some(origin) = headers.get(header::ORIGIN)
        && origin.as_bytes() != format!("http://{host}").as_bytes()
    {
        return ApiError::new(StatusCode::FORBIDDEN, "Cross-origin requests are disabled")
            .into_response();
    }
    if request.method() == Method::POST
        && !headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
            })
    {
        return ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Use application/json")
            .into_response();
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
    ));
    response
}

struct ApiError(StatusCode, String);

impl ApiError {
    fn new(status: StatusCode, error: impl Into<String>) -> Self {
        Self(status, error.into())
    }

    fn worker(error: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

#[derive(Serialize)]
struct WatchedFolder {
    path: String,
    inbox: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    permission_error: Option<String>,
}

fn watched_folders(runtime: &Runtime, config: &crate::config::Config) -> Vec<WatchedFolder> {
    let mut folders = BTreeMap::new();
    for source in config.sources.values() {
        for path in &source.roots {
            *folders.entry(path).or_insert(false) |= source.is_inbox;
        }
    }
    folders
        .into_iter()
        .map(|(path, inbox)| WatchedFolder {
            path: path.display().to_string(),
            inbox,
            permission_error: runtime.folder_access.error(path),
        })
        .collect()
}

async fn list_folders(State(runtime): State<Arc<Runtime>>) -> Json<Vec<WatchedFolder>> {
    Json(watched_folders(&runtime, &runtime.config()))
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum FolderAction {
    Add,
    Remove,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderChange {
    action: FolderAction,
    path: PathBuf,
}

async fn change_folder(
    State(runtime): State<Arc<Runtime>>,
    Json(change): Json<FolderChange>,
) -> Result<Json<Vec<WatchedFolder>>, ApiError> {
    update_folders(runtime, change, None).await.map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderPick {
    initial: Option<PathBuf>,
}

#[derive(Serialize)]
struct FolderPickResult {
    selected: bool,
    folders: Vec<WatchedFolder>,
}

async fn pick_folder(
    State(runtime): State<Arc<Runtime>>,
    Json(pick): Json<FolderPick>,
) -> Result<Json<FolderPickResult>, ApiError> {
    let initial = pick
        .initial
        .filter(|path| path.is_absolute() || path.starts_with("~"));
    let selection = runtime
        .folder_access
        .select(initial)
        .await
        .map_err(ApiError::worker)?;
    let Some(selection) = selection else {
        return Ok(Json(FolderPickResult {
            selected: false,
            folders: watched_folders(&runtime, &runtime.config()),
        }));
    };
    let authorized = tokio::task::spawn_blocking(move || selection.authorize())
        .await
        .map_err(ApiError::worker)?
        .map_err(|error| ApiError::new(StatusCode::FORBIDDEN, format!("{error:#}")))?;
    let change = FolderChange {
        action: FolderAction::Add,
        path: authorized.path.clone(),
    };
    let folders = update_folders(runtime, change, Some(authorized)).await?;
    Ok(Json(FolderPickResult {
        selected: true,
        folders,
    }))
}

async fn update_folders(
    runtime: Arc<Runtime>,
    change: FolderChange,
    authorized: Option<AuthorizedFolder>,
) -> Result<Vec<WatchedFolder>, ApiError> {
    tokio::task::spawn_blocking(move || {
        let _cloud = runtime.cloud_gate.lock().unwrap();
        if runtime.stopping.load(Ordering::Relaxed) {
            return Err(ApiError::worker("服务正在关闭，请重试。"));
        }
        let current = runtime.config();
        let mut config =
            crate::config::Config::read(&runtime.config_path).map_err(ApiError::worker)?;
        if !config.same_storage(&current) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "Restart kb after changing data_dir or port",
            ));
        }
        let store = Store::open(&config.data).map_err(ApiError::worker)?;
        let mut controls = store.folder_controls().map_err(ApiError::worker)?;

        let (path, enabled) = match change.action {
            FolderAction::Add => {
                let path = config
                    .resolve_folder(&runtime.config_path, &change.path)
                    .map_err(|error| {
                        ApiError::new(StatusCode::BAD_REQUEST, format!("{error:#}"))
                    })?;
                let canonical = path.canonicalize().map_err(ApiError::worker)?;
                let existing = current
                    .sources
                    .values()
                    .flat_map(|source| source.roots.iter())
                    .find(|root| {
                        **root == path
                            || root.canonicalize().is_ok_and(|actual| actual == canonical)
                    });
                let path = match existing {
                    Some(root) if authorized.is_some() => root.clone(),
                    Some(_) => {
                        return Err(ApiError::new(
                            StatusCode::CONFLICT,
                            "This folder is already watched",
                        ));
                    }
                    None => path,
                };
                let enabled = (!config.has_root(&path)).then_some(true);
                (path, enabled)
            }
            FolderAction::Remove => {
                let path = change.path;
                if !current.has_root(&path) {
                    return Err(ApiError::new(
                        StatusCode::NOT_FOUND,
                        "Watched folder not found",
                    ));
                }
                let enabled = config.has_root(&path).then_some(false);
                (path, enabled)
            }
        };

        let bookmark = authorized.as_ref().map(|folder| folder.bookmark.as_slice());
        store
            .set_folder_control(&path, enabled, bookmark)
            .map_err(ApiError::worker)?;
        if let Some(enabled) = enabled {
            controls.insert(path.clone(), enabled);
        } else {
            controls.remove(&path);
        }
        config.apply_folder_controls(&controls);
        if let Some(folder) = authorized {
            runtime.folder_access.remember(path, folder);
        } else if matches!(change.action, FolderAction::Remove) {
            runtime.folder_access.forget(&path);
        }
        let folders = watched_folders(&runtime, &config);
        *runtime.settings.write().unwrap() = Arc::new(config);
        runtime.scan_requested.store(true, Ordering::Release);
        Ok(folders)
    })
    .await
    .map_err(ApiError::worker)?
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UiTheme {
    Figma,
    Miro,
}

impl UiTheme {
    fn as_str(self) -> &'static str {
        match self {
            Self::Figma => "figma",
            Self::Miro => "miro",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeChange {
    theme: UiTheme,
}

#[derive(Serialize)]
struct ThemeResult {
    theme: &'static str,
}

async fn change_theme(
    State(runtime): State<Arc<Runtime>>,
    Json(change): Json<ThemeChange>,
) -> Result<Json<ThemeResult>, ApiError> {
    let theme = change.theme.as_str();
    run_blocking(move || {
        let config = runtime.config();
        let store = Store::open(&config.data)?;
        store.set_ui_theme(theme)?;
        Ok(ThemeResult { theme })
    })
    .await
    .map(Json)
}

#[derive(Serialize)]
struct ArchiveList {
    files: Vec<ArchivedDocument>,
}

async fn list_archive(State(runtime): State<Arc<Runtime>>) -> Result<Json<ArchiveList>, ApiError> {
    run_blocking(move || {
        let config = runtime.config();
        let store = Store::open(&config.data)?;
        Ok(ArchiveList {
            files: store.archived_files()?,
        })
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenRequest {
    hash: String,
    #[serde(default)]
    reveal: bool,
}

#[derive(Serialize)]
struct OpenResult {
    opened: &'static str,
    path: String,
}

async fn open_original(
    State(runtime): State<Arc<Runtime>>,
    Json(request): Json<OpenRequest>,
) -> Result<Json<OpenResult>, ApiError> {
    if request.hash.len() != 64
        || !request
            .hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Invalid file hash"));
    }
    let hash = request.hash;
    let reveal = request.reveal;
    let target = run_blocking(move || {
        let config = runtime.config();
        let store = Store::open(&config.data)?;
        store.resolve_existing_origin(&config.data, &hash)
    })
    .await?
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "原文件已不存在或无法访问，请点击“下载原文件”获取归档副本。",
        )
    })?;
    let launch_target = target.clone();
    run_blocking(move || launch_local_file(&launch_target, reveal)).await?;
    Ok(Json(OpenResult {
        opened: if reveal { "reveal" } else { "file" },
        path: target.display().to_string(),
    }))
}

fn launch_local_file(path: &std::path::Path, reveal: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    let output = {
        let mut command = std::process::Command::new("/usr/bin/open");
        if reveal {
            command.arg("-R");
        }
        command.arg("--").arg(path).output()?
    };
    #[cfg(target_os = "windows")]
    let output = {
        let mut command = std::process::Command::new("explorer.exe");
        if reveal {
            let mut arg = std::ffi::OsString::from("/select,");
            arg.push(path);
            command.arg(arg);
        } else {
            command.arg(path);
        }
        command.output()?
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let output = {
        let target = if reveal {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        std::process::Command::new("xdg-open")
            .arg(target)
            .output()?
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if stderr.is_empty() {
            anyhow::bail!("无法使用系统默认应用打开原文件。");
        }
        anyhow::bail!("无法打开原文件：{stderr}");
    }
    Ok(())
}

#[derive(Serialize)]
struct Status {
    counts: Counts,
    progress: Progress,
    errors: Vec<ParseError>,
    inbox: Option<String>,
    data_dir: String,
    sources: Vec<String>,
    cloud_enabled: bool,
    folder_picker: bool,
    theme: String,
}

async fn status(State(runtime): State<Arc<Runtime>>) -> Result<Json<Status>, ApiError> {
    run_blocking(move || {
        let config = runtime.config();
        let store = Store::open(&config.data)?;
        Ok(Status {
            counts: store.counts()?,
            progress: runtime.progress.read().unwrap().clone(),
            errors: store.errors()?,
            inbox: config
                .inboxes()
                .next()
                .map(|path| path.display().to_string()),
            data_dir: config.data.display().to_string(),
            sources: config.sources.keys().cloned().collect(),
            cloud_enabled: config.models.fallback.enabled
                && config.sources.values().any(|source| source.allow_cloud),
            folder_picker: runtime.folder_access.picker_available,
            theme: store.ui_theme()?,
        })
    })
    .await
    .map(Json)
}

async fn search_passages(
    State(runtime): State<Arc<Runtime>>,
    Json(request): Json<search::Request>,
) -> Result<Json<search::Results>, ApiError> {
    request
        .validate()
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;
    let permit =
        runtime.requests.clone().try_acquire_owned().map_err(|_| {
            ApiError::new(StatusCode::TOO_MANY_REQUESTS, "The knowledge base is busy")
        })?;
    let state = runtime.clone();
    run_blocking(move || {
        let _permit = permit;
        let config = state.config();
        let store = Store::open(&config.data)?;
        search::retrieve(&state, &config, &store, &request)
    })
    .await
    .map(Json)
}

#[derive(Serialize)]
struct WebAnswer {
    #[serde(flatten)]
    answer: models::Answer,
    html: String,
}

async fn ask(
    State(runtime): State<Arc<Runtime>>,
    Json(request): Json<models::Ask>,
) -> Result<Json<WebAnswer>, ApiError> {
    if request.question.trim().is_empty()
        || request.question.trim().len() > crate::config::MAX_QUERY_BYTES
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Question is empty or exceeds 2000 UTF-8 bytes",
        ));
    }
    let permit =
        runtime.requests.clone().try_acquire_owned().map_err(|_| {
            ApiError::new(StatusCode::TOO_MANY_REQUESTS, "The knowledge base is busy")
        })?;
    let state = runtime.clone();
    run_blocking(move || {
        let _permit = permit;
        let config = state.config();
        let store = Store::open(&config.data)?;
        let answer = models::ask(&state, &config, &store, &request)?;
        let html = markdown::render(&answer.text);
        Ok(WebAnswer { answer, html })
    })
    .await
    .map(Json)
}

async fn scan(
    State(runtime): State<Arc<Runtime>>,
    Json(_): Json<Value>,
) -> (StatusCode, Json<Value>) {
    runtime.retry_requested.store(true, Ordering::Relaxed);
    runtime.scan_requested.store(true, Ordering::Relaxed);
    (StatusCode::ACCEPTED, Json(json!({"queued": "scan"})))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryRequest {
    hash: Option<String>,
}

#[derive(Serialize)]
struct RetryResult {
    queued: &'static str,
    retried: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
}

async fn retry_failed(
    State(runtime): State<Arc<Runtime>>,
    Json(request): Json<RetryRequest>,
) -> Result<(StatusCode, Json<RetryResult>), ApiError> {
    if let Some(hash) = &request.hash
        && (hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Invalid file hash"));
    }
    let state = runtime.clone();
    let target = request.hash.clone();
    let retried = run_blocking(move || {
        let config = state.config();
        let store = Store::open(&config.data)?;
        store.retry_failed(target.as_deref())
    })
    .await?
    .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Archived file not found"))?;
    if request.hash.is_none() {
        runtime.retry_requested.store(true, Ordering::Relaxed);
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(RetryResult {
            queued: "retry",
            retried,
            hash: request.hash,
        }),
    ))
}

async fn reindex(
    State(runtime): State<Arc<Runtime>>,
    Json(_): Json<Value>,
) -> (StatusCode, Json<Value>) {
    runtime.reindex_requested.store(true, Ordering::Relaxed);
    (StatusCode::ACCEPTED, Json(json!({"queued": "reindex"})))
}

async fn pdf_preview(
    State(runtime): State<Arc<Runtime>>,
    Path((hash, page)): Path<(String, u32)>,
) -> Result<Response, ApiError> {
    let png = with_pdf_page(runtime, hash, page, worker::pdf_preview).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (
                header::HeaderName::from_static("cross-origin-resource-policy"),
                "same-origin",
            ),
        ],
        png,
    )
        .into_response())
}

async fn pdf_tables(
    State(runtime): State<Arc<Runtime>>,
    Path((hash, page)): Path<(String, u32)>,
) -> Result<Json<Vec<worker::PdfTable>>, ApiError> {
    with_pdf_page(runtime, hash, page, worker::pdf_tables)
        .await
        .map(Json)
}

async fn image_preview(
    State(runtime): State<Arc<Runtime>>,
    Path(hash): Path<String>,
) -> Result<Response, ApiError> {
    let permit =
        runtime.requests.clone().try_acquire_owned().map_err(|_| {
            ApiError::new(StatusCode::TOO_MANY_REQUESTS, "The knowledge base is busy")
        })?;
    let png = run_blocking(move || {
        let _permit = permit;
        let config = runtime.config();
        let record = resolve_archive(&config.data, &hash)?;
        record
            .filter(|(path, _)| {
                path.extension()
                    .and_then(|ext| ext.to_str())
                    .map(str::to_ascii_lowercase)
                    .is_some_and(|ext| crate::store::is_image_extension(&ext))
            })
            .map(|(path, _)| worker::image_preview(&config, &path))
            .transpose()
    })
    .await?
    .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Image not found"))?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (
                header::HeaderName::from_static("cross-origin-resource-policy"),
                "same-origin",
            ),
        ],
        png,
    )
        .into_response())
}

async fn document_preview(
    State(runtime): State<Arc<Runtime>>,
    Path(hash): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let permit =
        runtime.requests.clone().try_acquire_owned().map_err(|_| {
            ApiError::new(StatusCode::TOO_MANY_REQUESTS, "The knowledge base is busy")
        })?;
    let payload = run_blocking(move || {
        let _permit = permit;
        let config = runtime.config();
        let Some((path, title)) = resolve_archive(&config.data, &hash)? else {
            return Ok(None);
        };
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if extension == "pdf" {
            return Ok(None);
        }
        if crate::store::is_image_extension(&extension) {
            let store = Store::open(&config.data)?;
            let passages: Vec<Value> = store
                .document_passages(&hash, 20)?
                .into_iter()
                .map(|(locator, text)| json!({"locator": locator, "text": text}))
                .collect();
            return Ok(Some(json!({
                "kind": "image",
                "title": title,
                "extension": extension,
                "image_url": format!("/preview/{hash}/image"),
                "passages": passages,
                "truncated": false,
            })));
        }
        let mut preview = worker::document_preview(&config, &path)?;
        if let Some(object) = preview.as_object_mut() {
            object.insert("title".into(), json!(title));
            object.insert("extension".into(), json!(extension));
            if object.get("kind").and_then(Value::as_str) == Some("markdown") {
                let raw = object
                    .remove("markdown")
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default();
                object.insert("html".into(), json!(markdown::render_document(&raw)));
            }
        }
        Ok(Some(preview))
    })
    .await?
    .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Document preview not found"))?;
    Ok(Json(payload))
}

async fn with_pdf_page<T: Send + 'static>(
    runtime: Arc<Runtime>,
    hash: String,
    page: u32,
    action: fn(&crate::config::Config, &std::path::Path, u32) -> Result<T>,
) -> Result<T, ApiError> {
    if page == 0 {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "PDF page not found"));
    }
    let permit =
        runtime.requests.clone().try_acquire_owned().map_err(|_| {
            ApiError::new(StatusCode::TOO_MANY_REQUESTS, "The knowledge base is busy")
        })?;
    run_blocking(move || {
        let _permit = permit;
        let config = runtime.config();
        let record = resolve_archive(&config.data, &hash)?;
        record
            .filter(|(path, _)| {
                path.extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
            })
            .map(|(path, _)| action(&config, &path, page))
            .transpose()
    })
    .await?
    .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "PDF not found"))
}

async fn original(
    State(runtime): State<Arc<Runtime>>,
    Path(hash): Path<String>,
) -> Result<Response, ApiError> {
    let record = run_blocking(move || resolve_archive(&runtime.config().data, &hash)).await?;
    let (path, title) =
        record.ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Original file not found"))?;
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "Original file not found"))?;
    let length = file.metadata().await.map_err(ApiError::worker)?.len();
    let filename = utf8_percent_encode(&title, NON_ALPHANUMERIC);
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".into()),
            (header::CONTENT_LENGTH, length.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename*=UTF-8''{filename}"),
            ),
        ],
        Body::from_stream(ReaderStream::new(file)),
    )
        .into_response())
}

async fn run_blocking<T: Send + 'static>(
    function: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(function)
        .await
        .map_err(ApiError::worker)?
        .map_err(|error| ApiError::worker(format!("{error:#}")))
}
