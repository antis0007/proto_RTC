use std::{collections::HashMap, fs, net::SocketAddr, sync::Arc};

use axum::{
    body::Bytes,
    extract::{Path, Query, State, WebSocketUpgrade},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use livekit_integration::LiveKitConfig;
use serde::{Deserialize, Serialize};
use server_api::{list_channels, list_guilds, send_message, ApiContext};
use shared::{
    domain::{ChannelId, FileId, GuildId, UserId},
    error::{ApiError, ErrorCode},
    protocol::ServerEvent,
};
use storage::Storage;
use tokio::sync::broadcast;
use tracing::info;

#[derive(Debug, Deserialize)]
struct Settings {
    bind_addr: String,
    database_url: String,
    livekit_api_key: String,
    livekit_api_secret: String,
    livekit_ttl_seconds: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:8080".into(),
            database_url: "sqlite://community.db".into(),
            livekit_api_key: "devkey".into(),
            livekit_api_secret: "devsecret".into(),
            livekit_ttl_seconds: 3600,
        }
    }
}

#[derive(Clone)]
struct AppState {
    api: ApiContext,
    events: broadcast::Sender<ServerEvent>,
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    username: String,
}

#[derive(Debug, Serialize)]
struct LoginResponse {
    user_id: i64,
}

#[derive(Debug, Deserialize)]
struct FileUploadQuery {
    user_id: i64,
    guild_id: i64,
    channel_id: i64,
}

#[derive(Debug, Deserialize)]
struct WsQuery {
    user_id: i64,
}

#[derive(Debug, Deserialize)]
struct UserQuery {
    user_id: i64,
}

#[derive(Debug, Deserialize)]
struct SendMessageRequest {
    user_id: i64,
    guild_id: i64,
    channel_id: i64,
    ciphertext_b64: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let settings = load_settings();
    let storage = Storage::new(&settings.database_url).await?;
    let api = ApiContext {
        storage,
        livekit: LiveKitConfig {
            api_key: settings.livekit_api_key,
            api_secret: settings.livekit_api_secret,
            ttl_seconds: settings.livekit_ttl_seconds,
        },
    };
    let (events, _) = broadcast::channel(256);

    let state = AppState { api, events };
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/login", post(login))
        .route("/guilds", get(http_list_guilds))
        .route("/guilds/:guild_id/channels", get(http_list_channels))
        .route("/messages", post(http_send_message))
        .route("/files/upload", post(upload_file))
        .route("/files/:file_id", get(download_file))
        .route("/ws", get(ws_handler))
        .with_state(Arc::new(state));

    let addr: SocketAddr = settings.bind_addr.parse()?;
    info!(%addr, "server listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn load_settings() -> Settings {
    let mut settings = Settings::default();
    if let Ok(raw) = fs::read_to_string("server.toml") {
        if let Ok(file_cfg) = toml::from_str::<HashMap<String, String>>(&raw) {
            if let Some(v) = file_cfg.get("bind_addr") {
                settings.bind_addr = v.clone();
            }
            if let Some(v) = file_cfg.get("database_url") {
                settings.database_url = v.clone();
            }
        }
    }

    if let Ok(v) = std::env::var("APP__BIND_ADDR") {
        settings.bind_addr = v;
    }
    if let Ok(v) = std::env::var("APP__DATABASE_URL") {
        settings.database_url = v;
    }
    if let Ok(v) = std::env::var("APP__LIVEKIT_API_KEY") {
        settings.livekit_api_key = v;
    }
    if let Ok(v) = std::env::var("APP__LIVEKIT_API_SECRET") {
        settings.livekit_api_secret = v;
    }
    if let Ok(v) = std::env::var("APP__LIVEKIT_TTL_SECONDS") {
        if let Ok(parsed) = v.parse::<i64>() {
            settings.livekit_ttl_seconds = parsed;
        }
    }
    settings
}

async fn healthz() -> &'static str {
    "ok"
}

async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, (StatusCode, Json<ApiError>)> {
    let user_id = state
        .api
        .storage
        .create_user(&req.username)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ApiError::new(ErrorCode::Validation, e.to_string())),
            )
        })?;

    Ok(Json(LoginResponse { user_id: user_id.0 }))
}

async fn upload_file(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FileUploadQuery>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    let file_id = state
        .api
        .storage
        .store_file_ciphertext(
            UserId(q.user_id),
            GuildId(q.guild_id),
            ChannelId(q.channel_id),
            &body,
            Some("application/octet-stream"),
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?;
    Ok(Json(serde_json::json!({ "file_id": file_id.0 })))
}

async fn download_file(
    State(state): State<Arc<AppState>>,
    Path(file_id): Path<i64>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    let bytes = state
        .api
        .storage
        .load_file_ciphertext(FileId(file_id))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new(ErrorCode::NotFound, "file not found")),
            )
        })?;
    Ok((StatusCode::OK, bytes))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Query(q): Query<WsQuery>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_connection(state, socket, UserId(q.user_id)))
}

async fn ws_connection(
    state: Arc<AppState>,
    socket: axum::extract::ws::WebSocket,
    _user_id: UserId,
) {
    use axum::extract::ws::Message;
    use futures::{SinkExt, StreamExt};

    let (mut sender, mut receiver) = socket.split();
    let mut events_rx = state.events.subscribe();

    let send_task = tokio::spawn(async move {
        while let Ok(event) = events_rx.recv().await {
            let text = match serde_json::to_string(&event) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if sender.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(_msg)) = receiver.next().await {}

    send_task.abort();
}

async fn http_list_guilds(
    State(state): State<Arc<AppState>>,
    Query(q): Query<UserQuery>,
) -> Result<Json<Vec<shared::protocol::GuildSummary>>, (StatusCode, Json<ApiError>)> {
    let guilds = list_guilds(&state.api, UserId(q.user_id))
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(e)))?;
    Ok(Json(guilds))
}

async fn http_list_channels(
    State(state): State<Arc<AppState>>,
    Path(guild_id): Path<i64>,
    Query(q): Query<UserQuery>,
) -> Result<Json<Vec<shared::protocol::ChannelSummary>>, (StatusCode, Json<ApiError>)> {
    let channels = list_channels(&state.api, UserId(q.user_id), GuildId(guild_id))
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(e)))?;
    Ok(Json(channels))
}

async fn http_send_message(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SendMessageRequest>,
) -> Result<Json<ServerEvent>, (StatusCode, Json<ApiError>)> {
    let event = send_message(
        &state.api,
        UserId(req.user_id),
        GuildId(req.guild_id),
        ChannelId(req.channel_id),
        &req.ciphertext_b64,
    )
    .await
    .map_err(|e| (StatusCode::BAD_REQUEST, Json(e)))?;
    let _ = state.events.send(event.clone());
    Ok(Json(event))
}
