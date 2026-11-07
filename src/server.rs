use crate::api::{NotAuthenticated, XClient};
use crate::auth::{self, TokenStore};
use crate::config::Config;
use crate::models::{CallbackQuery, User};
use crate::store::Store;
use crate::sync::{run_sync, SyncHandle};
use crate::topics;
use anyhow::Result;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::sync::{Arc, Mutex};

pub struct AppState {
    pub cfg: Config,
    pub store: Store,
    pub client: XClient,
    pub sync: Arc<SyncHandle>,
    pub pending_auth: Mutex<Option<PendingAuth>>,
    pub me: Mutex<Option<User>>,
}

pub struct PendingAuth {
    state: String,
    verifier: String,
}

struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.0.downcast_ref::<NotFound>().is_some() {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": self.0.to_string()})),
            )
                .into_response();
        }
        if self.0.downcast_ref::<NotAuthenticated>().is_some() {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": self.0.to_string()})),
            )
                .into_response();
        }
        tracing::error!("handler error: {:#}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{:#}", self.0)})),
        )
            .into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(err: E) -> Self {
        ApiError(err.into())
    }
}

#[derive(Debug)]
struct NotFound;

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not found")
    }
}
impl std::error::Error for NotFound {}

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/style.css", get(css))
        .route("/app.js", get(js))
        .route("/api/status", get(api_status))
        .route("/api/bookmarks", get(api_bookmarks))
        .route("/api/bookmarks/{id}", get(api_bookmark_detail))
        .route("/api/topics", get(api_topics))
        .route("/api/topics/export", get(api_topics_export))
        .route("/api/sync", post(api_sync))
        .route("/api/login/start", post(api_login_start))
        .route("/callback", get(api_callback))
        .route("/api/export", get(api_export))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    let mut resp = Response::new(axum::body::Body::from(body));
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp
}

async fn css() -> Response {
    asset(include_str!("../web/style.css"), "text/css; charset=utf-8")
}

async fn js() -> Response {
    asset(
        include_str!("../web/app.js"),
        "application/javascript; charset=utf-8",
    )
}

async fn api_status(State(st): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiError> {
    let authenticated = st.client.tokens().get().is_some();
    let total = st.store.count_bookmarks()?;
    let sync = st.sync.snapshot();
    Ok(Json(json!({
        "authenticated": authenticated,
        "user": st.me.lock().unwrap().clone(),
        "total_bookmarks": total,
        "last_sync_at": st.store.get_meta("last_sync_at")?,
        "last_full_sync_at": st.store.get_meta("last_full_sync_at")?,
        "sync": sync,
        "callback_url": st.cfg.callback_url,
        "client_id_set": st.cfg.client_id.is_some(),
        "paths": {
            "database": st.cfg.db_path(),
            "jsonl": st.cfg.jsonl_path(),
            "markdown": st.cfg.md_path(),
        },
    })))
}

#[derive(Deserialize)]
struct ListParams {
    q: Option<String>,
    domain: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn api_bookmarks(
    State(st): State<Arc<AppState>>,
    Query(p): Query<ListParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let offset = p.offset.unwrap_or(0).max(0);
    let (items, total) = st.store.list_bookmarks(
        p.q.as_deref().map(|q| q.trim()).filter(|q| !q.is_empty()),
        p.domain
            .as_deref()
            .map(|d| d.trim())
            .filter(|d| !d.is_empty()),
        limit,
        offset,
    )?;
    Ok(Json(json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        "items": items,
    })))
}

async fn api_bookmark_detail(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some((view, raw)) = st.store.get_view(&id)? else {
        return Err(ApiError(
            anyhow::Error::new(NotFound).context(format!("no bookmark with id {id}")),
        ));
    };
    Ok(Json(json!({"item": view, "raw": raw})))
}

#[derive(Deserialize)]
struct TopicsParams {
    domain: Option<String>,
    /// local (bookmarks archive, default) or x (live search across all of X)
    scope: Option<String>,
    /// For scope=x: max posts to fetch.
    max_posts: Option<u64>,
}

fn required_input(raw: Option<&str>) -> Result<String, Box<Response>> {
    let Some(input) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Err(Box::new(
            (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "provide a domain (e.g. ?domain=arxiv.org) or a topic word (e.g. ?domain=rust)"
                })),
            )
                .into_response(),
        ));
    };
    Ok(input.to_string())
}

fn topics_error_response(e: anyhow::Error) -> Response {
    if e.downcast_ref::<NotAuthenticated>().is_some() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": format!("{e:#}")})),
        )
            .into_response();
    }
    tracing::error!("topic analysis failed: {e:#}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": format!("{e:#}")})),
    )
        .into_response()
}

async fn run_topics_analysis(
    st: &Arc<AppState>,
    input: &str,
    scope: &str,
    max_posts: Option<u64>,
) -> anyhow::Result<topics::TopicsResult> {
    match scope {
        "x" => {
            let max = max_posts.unwrap_or(st.cfg.max_search_posts).min(1000);
            let progress: &(dyn Fn(String) + Send + Sync) = &|msg: String| tracing::info!("{msg}");
            topics::analyze_x(&st.client, input, max, st.cfg.search_page_size, progress).await
        }
        "local" => topics::analyze(&st.store, input),
        other => Err(anyhow::anyhow!("unknown scope {other:?} (use local or x)")),
    }
}

/// GET /api/topics?domain=<…>&scope=local|x
async fn api_topics(
    State(st): State<Arc<AppState>>,
    Query(p): Query<TopicsParams>,
) -> Result<Json<serde_json::Value>, Response> {
    let input = required_input(p.domain.as_deref()).map_err(|r| *r)?;
    let scope = p.scope.as_deref().unwrap_or("local");
    let result = run_topics_analysis(&st, &input, scope, p.max_posts)
        .await
        .map_err(topics_error_response)?;
    Ok(Json(serde_json::to_value(&result.report).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response()
    })?))
}

#[derive(Deserialize)]
struct TopicsExportParams {
    domain: Option<String>,
    format: Option<String>,
    scope: Option<String>,
    max_posts: Option<u64>,
}

/// GET /api/topics/export?domain=<…>&format=json|csv|md
async fn api_topics_export(
    State(st): State<Arc<AppState>>,
    Query(p): Query<TopicsExportParams>,
) -> Result<Response, Response> {
    let input = required_input(p.domain.as_deref()).map_err(|r| *r)?;
    let scope = p.scope.as_deref().unwrap_or("local");
    let result = run_topics_analysis(&st, &input, scope, p.max_posts)
        .await
        .map_err(topics_error_response)?;
    let (body, content_type, filename): (String, &'static str, String) =
        match p.format.as_deref().unwrap_or("json") {
            "csv" => (
                topics::export_csv(&result.report),
                "text/csv; charset=utf-8",
                format!("topics-{}.csv", topics::normalize(&input).replace('.', "_")),
            ),
            "md" | "markdown" => (
                topics::export_md(&result),
                "text/markdown; charset=utf-8",
                format!("topics-{}.md", topics::normalize(&input).replace('.', "_")),
            ),
            _ => (
                serde_json::to_string_pretty(&topics::export_json(&result)).map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": e.to_string()})),
                    )
                        .into_response()
                })?,
                "application/json",
                format!(
                    "topics-{}.json",
                    topics::normalize(&input).replace('.', "_")
                ),
            ),
        };
    let mut resp = Response::new(axum::body::Body::from(body));
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Ok(cd) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, cd);
    }
    Ok(resp)
}

async fn api_sync(State(st): State<Arc<AppState>>) -> Response {
    if st.sync.is_running() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "a sync is already running"})),
        )
            .into_response();
    }
    let st2 = Arc::clone(&st);
    tokio::spawn(async move {
        let st3 = Arc::clone(&st2);
        let res = run_sync(
            &st2.client,
            &st2.store,
            &st2.cfg,
            &st2.sync,
            false,
            move |me| *st3.me.lock().unwrap() = Some(me),
        )
        .await;
        if let Err(e) = res {
            tracing::error!("sync failed: {e:#}");
        }
    });
    (StatusCode::ACCEPTED, Json(json!({"started": true}))).into_response()
}

async fn api_login_start(
    State(st): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, Response> {
    let Some(client_id) = st.cfg.client_id.clone() else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "client_id is not configured. Set X_CLIENT_ID or client_id in {} (see README for the developer.x.com setup).",
                    st.cfg.config_path().display()
                )
            })),
        )
            .into_response());
    };
    let pkce = auth::generate_pkce();
    let url = auth::authorize_url(
        &st.cfg.authorize_url,
        &client_id,
        &st.cfg.callback_url,
        auth::SCOPE,
        &pkce.state,
        &pkce.challenge,
    )
    .map_err(|e| {
        tracing::error!("failed to build authorize URL: {e:#}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("failed to build authorize URL: {e:#}")})),
        )
            .into_response()
    })?;
    *st.pending_auth.lock().unwrap() = Some(PendingAuth {
        state: pkce.state,
        verifier: pkce.verifier,
    });
    Ok(Json(json!({"url": url})))
}

async fn api_callback(State(st): State<Arc<AppState>>, Query(q): Query<CallbackQuery>) -> Response {
    let pending = st.pending_auth.lock().unwrap().take();
    let Some(pending) = pending else {
        return Redirect::to("/?login=error&reason=no_pending_login").into_response();
    };

    let code = match auth::check_callback(&q, &pending.state) {
        Ok(code) => code,
        Err(e) => {
            return Redirect::to(&format!(
                "/?login=error&reason={}",
                auth::urlencode(&e.to_string())
            ))
            .into_response()
        }
    };

    // X authorization codes expire ~30 seconds after issuance, so the
    // exchange must happen right here.
    let tokens = match auth::exchange_code(
        st.client.http(),
        st.client.token_url(),
        st.client.client_id(),
        st.client.client_secret(),
        st.client.redirect_uri(),
        &code,
        &pending.verifier,
    )
    .await
    {
        Ok(tokens) => tokens,
        Err(e) => {
            tracing::error!("code exchange failed: {e:#}");
            return Redirect::to(&format!(
                "/?login=error&reason={}",
                auth::urlencode(&e.to_string())
            ))
            .into_response();
        }
    };

    if let Err(e) = st.client.tokens().set(tokens) {
        tracing::error!("failed to persist tokens: {e:#}");
        return Redirect::to(&format!(
            "/?login=error&reason={}",
            auth::urlencode("could not save tokens")
        ))
        .into_response();
    }
    if let Ok(me) = st.client.get_me().await {
        *st.me.lock().unwrap() = Some(me);
    }
    Redirect::to("/?login=success").into_response()
}

#[derive(Deserialize)]
struct ExportParams {
    format: Option<String>,
}

async fn api_export(
    State(st): State<Arc<AppState>>,
    Query(p): Query<ExportParams>,
) -> Result<Response, ApiError> {
    let mut body: Vec<u8> = Vec::new();
    let (content_type, filename) = match p.format.as_deref().unwrap_or("jsonl") {
        "md" | "markdown" => {
            st.store.export_md(&mut body)?;
            ("text/markdown; charset=utf-8", "bookmarks.md")
        }
        _ => {
            st.store.export_jsonl(&mut body)?;
            ("application/x-ndjson", "bookmarks.jsonl")
        }
    };
    let mut resp = Response::new(axum::body::Body::from(body));
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Ok(cd) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, cd);
    }
    Ok(resp)
}

/// Spin up the server with everything wired. Returns only when the server stops.
pub async fn serve(cfg: Config, open_browser: bool) -> Result<()> {
    let store = Store::open(&cfg.db_path())?;
    let tokens = Arc::new(TokenStore::load(cfg.tokens_path()));
    let client = XClient::new(&cfg, tokens.clone());
    let state = Arc::new(AppState {
        cfg: cfg.clone(),
        store,
        client,
        sync: Arc::new(SyncHandle::new()),
        pending_auth: Mutex::new(None),
        me: Mutex::new(None),
    });

    // Warm the "authenticated user" cache if we have tokens.
    if tokens.get().is_some() {
        let st = Arc::clone(&state);
        tokio::spawn(async move {
            if let Ok(me) = st.client.get_me().await {
                *st.me.lock().unwrap() = Some(me);
            }
        });
    }

    let addr = cfg.listen_addr();
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("Xanalysis running at http://localhost:{}", cfg.port);
    println!("  config: {}", cfg.config_path().display());
    println!("  data:   {}", cfg.data_dir().display());
    if open_browser {
        let url = format!("http://localhost:{}", cfg.port);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            let _ = open::that(url);
        });
    }
    axum::serve(listener, build_router(state)).await?;
    Ok(())
}
