use crate::{
    engine::{
        config::EngineConfig,
        manager::DownloadManager,
        task::{TaskId, TaskOptions},
    },
    proto::types::{
        self, AddOptions, AddUriRequest, GlobalStatResponse, OkResponse, StatusResponse,
    },
};
use axum::{
    extract::{ws::WebSocket, Path, State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{delete, get, post, put},
    Router,
};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use url::Url;

/// Shared state injected into every handler.
#[derive(Clone)]
pub struct AppState {
    pub manager: DownloadManager,
    pub config: EngineConfig,
}

/// Build and return the axum Router.
pub fn router(state: AppState) -> Router {
    Router::new()
        // ── Download management ────────────────────────────────────────────
        .route("/api/addUri", post(add_uri))
        .route("/api/tellStatus/:gid", get(tell_status))
        .route("/api/tellActive", get(tell_active))
        .route("/api/tellWaiting", get(tell_waiting))
        .route("/api/tellStopped", get(tell_stopped))
        .route("/api/pause/:gid", put(pause))
        .route("/api/resume/:gid", put(resume))
        .route("/api/remove/:gid", delete(remove))
        .route("/api/getGlobalStat", get(global_stat))
        // ── WebSocket live progress ────────────────────────────────────────
        .route("/ws/progress/:gid", get(ws_progress))
        .route("/ws/global", get(ws_global))
        // ── Config ────────────────────────────────────────────────────────
        .route("/api/config", get(get_config))
        .layer(CorsLayer::permissive())
        .with_state(Arc::new(state))
}

/// Start the RPC server on configured host:port.
pub async fn serve(state: AppState) -> anyhow::Result<()> {
    let addr = format!(
        "{}:{}",
        state.config.rpc.host, state.config.rpc.port
    );
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("RPC server listening on http://{}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

/// POST /api/addUri
async fn add_uri(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AddUriRequest>,
) -> Result<Json<Value>, ApiError> {
    let urls: Vec<Url> = req
        .uris
        .iter()
        .map(|u| Url::parse(u).map_err(|e| ApiError::bad_request(e.to_string())))
        .collect::<Result<_, _>>()?;

    let options = options_from_add(&req.options);
    let id = state.manager.add(urls, options)?;

    Ok(Json(json!({ "gid": id.0 })))
}

/// GET /api/tellStatus/:gid
async fn tell_status(
    State(state): State<Arc<AppState>>,
    Path(gid): Path<String>,
) -> Result<Json<StatusResponse>, ApiError> {
    let id = TaskId(gid);
    let task = state
        .manager
        .get(&id)
        .ok_or_else(|| ApiError::not_found("Task not found"))?;
    let progress = state
        .manager
        .subscribe(&id)
        .and_then(|rx| rx.borrow().clone());
    Ok(Json(types::task_to_status(&task, progress.as_ref())))
}

/// GET /api/tellActive
async fn tell_active(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<StatusResponse>> {
    let tasks: Vec<_> = state
        .manager
        .list()
        .into_iter()
        .filter(|t| t.state.is_active())
        .map(|t| {
            let progress = state
                .manager
                .subscribe(&t.id)
                .and_then(|rx| rx.borrow().clone());
            types::task_to_status(&t, progress.as_ref())
        })
        .collect();
    Json(tasks)
}

/// GET /api/tellWaiting
async fn tell_waiting(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<StatusResponse>> {
    use crate::engine::task::DownloadState;
    let tasks: Vec<_> = state
        .manager
        .list()
        .into_iter()
        .filter(|t| matches!(t.state, DownloadState::Queued | DownloadState::Paused))
        .map(|t| types::task_to_status(&t, None))
        .collect();
    Json(tasks)
}

/// GET /api/tellStopped
async fn tell_stopped(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<StatusResponse>> {
    let tasks: Vec<_> = state
        .manager
        .list()
        .into_iter()
        .filter(|t| t.state.is_terminal())
        .map(|t| types::task_to_status(&t, None))
        .collect();
    Json(tasks)
}

/// PUT /api/pause/:gid
async fn pause(
    State(state): State<Arc<AppState>>,
    Path(gid): Path<String>,
) -> Result<Json<OkResponse>, ApiError> {
    state.manager.pause(&TaskId(gid))?;
    Ok(Json(OkResponse::ok()))
}

/// PUT /api/resume/:gid
async fn resume(
    State(state): State<Arc<AppState>>,
    Path(gid): Path<String>,
) -> Result<Json<OkResponse>, ApiError> {
    state.manager.resume(&TaskId(gid))?;
    Ok(Json(OkResponse::ok()))
}

/// DELETE /api/remove/:gid
async fn remove(
    State(state): State<Arc<AppState>>,
    Path(gid): Path<String>,
) -> Result<Json<OkResponse>, ApiError> {
    state.manager.cancel(&TaskId(gid), false)?;
    Ok(Json(OkResponse::ok()))
}

/// GET /api/getGlobalStat
async fn global_stat(
    State(state): State<Arc<AppState>>,
) -> Json<GlobalStatResponse> {
    use crate::engine::task::DownloadState;
    let tasks = state.manager.list();
    let num_active = tasks.iter().filter(|t| t.state.is_active()).count() as u32;
    let num_waiting = tasks
        .iter()
        .filter(|t| matches!(t.state, DownloadState::Queued | DownloadState::Paused))
        .count() as u32;
    let num_stopped = tasks.iter().filter(|t| t.state.is_terminal()).count() as u32;

    Json(GlobalStatResponse {
        download_speed: "0".to_string(),
        num_active,
        num_waiting,
        num_stopped,
    })
}

/// GET /api/config
async fn get_config(
    State(state): State<Arc<AppState>>,
) -> Json<EngineConfig> {
    Json(state.config.clone())
}

// ─── WebSocket handlers ───────────────────────────────────────────────────────

/// GET /ws/progress/:gid  — streams DownloadProgress JSON over WebSocket
async fn ws_progress(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(gid): Path<String>,
) -> Response {
    ws.on_upgrade(move |socket| handle_ws_progress(socket, state, TaskId(gid)))
}

async fn handle_ws_progress(
    mut socket: WebSocket,
    state: Arc<AppState>,
    id: TaskId,
) {
    use axum::extract::ws::Message;
    use tokio::time::{interval, Duration};

    let Some(mut rx) = state.manager.subscribe(&id) else {
        let _ = socket.send(Message::Text(r#"{"error":"task not found"}"#.into())).await;
        return;
    };

    let mut ticker = interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let progress = rx.borrow().clone();
                if let Some(p) = progress {
                    if let Ok(json) = serde_json::to_string(&p) {
                        if socket.send(Message::Text(json)).await.is_err() {
                            break;
                        }
                    }
                    if p.state.is_terminal() {
                        break;
                    }
                }
            }
            msg = socket.recv() => {
                // Client disconnect
                if msg.is_none() { break; }
            }
        }
    }
}

/// GET /ws/global — streams aggregate stats (speed, queue lengths)
async fn ws_global(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_ws_global(socket, state))
}

async fn handle_ws_global(mut socket: WebSocket, state: Arc<AppState>) {
    use axum::extract::ws::Message;
    use crate::engine::task::DownloadState;
    use tokio::time::{interval, Duration};

    let mut ticker = interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let tasks = state.manager.list();
                let active = tasks.iter().filter(|t| t.state.is_active()).count();
                let waiting = tasks.iter()
                    .filter(|t| matches!(t.state, DownloadState::Queued))
                    .count();
                let payload = serde_json::json!({
                    "active": active,
                    "waiting": waiting,
                    "stopped": tasks.iter().filter(|t| t.state.is_terminal()).count(),
                });
                if socket.send(Message::Text(payload.to_string())).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => {
                if msg.is_none() { break; }
            }
        }
    }
}

// ─── Error type ───────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn bad_request(msg: impl ToString) -> Self {
        Self { status: StatusCode::BAD_REQUEST, message: msg.to_string() }
    }
    pub fn not_found(msg: impl ToString) -> Self {
        Self { status: StatusCode::NOT_FOUND, message: msg.to_string() }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: e.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(json!({ "error": self.message }));
        (self.status, body).into_response()
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn options_from_add(o: &AddOptions) -> TaskOptions {
    TaskOptions {
        out: o.out.clone(),
        dir: o.dir.as_deref().map(PathBuf::from),
        split: o.split,
        max_connections: o.max_connection_per_server,
        sequential: o.sequential_download.unwrap_or(false),
        max_speed: o.max_download_limit.unwrap_or(0),
        headers: o
            .header
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .filter_map(|h| {
                let mut parts = h.splitn(2, ':');
                Some((
                    parts.next()?.trim().to_string(),
                    parts.next()?.trim().to_string(),
                ))
            })
            .collect(),
        referer: o.referer.clone(),
        ..Default::default()
    }
}
