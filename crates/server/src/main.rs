use std::{net::SocketAddr, sync::Arc};

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
use server_api::{list_channels, list_guilds, request_livekit_token, send_message, ApiContext};
use shared::{
    domain::{ChannelId, FileId, GuildId, UserId},
    error::{ApiError, ErrorCode},
    protocol::{ClientRequest, ServerEvent},
};
use storage::Storage;
use tokio::sync::broadcast;
use tracing::info;

mod config;

use config::{load_settings, prepare_database_url};

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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let settings = load_settings();
    info!(
        server_public_url = ?settings.server_public_url,
        livekit_url = ?settings.livekit_url,
        "loaded server settings"
    );
    let database_url = prepare_database_url(&settings.database_url)?;
    let storage = Storage::new(&database_url).await.map_err(|error| {
        tracing::error!(
            %database_url,
            %error,
            "failed to open SQLite database; verify parent directory exists and permissions are correct"
        );
        error
    })?;
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
        .route("/files/upload", post(upload_file))
        .route("/files/:file_id", get(download_file))
        .route("/ws", get(ws_handler))
        .with_state(Arc::new(state));

    let addr: SocketAddr = settings.server_bind.parse()?;
    info!(%addr, "server listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
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
    user_id: UserId,
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

    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Text(text) = msg {
            match serde_json::from_str::<ClientRequest>(&text) {
                Ok(req) => {
                    let evt = handle_ws_request(&state, user_id, req).await;
                    if let Some(evt) = evt {
                        let _ = state.events.send(evt);
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "invalid WS request");
                }
            }
        }
    }

    send_task.abort();
}

async fn handle_ws_request(
    state: &AppState,
    user_id: UserId,
    req: ClientRequest,
) -> Option<ServerEvent> {
    match req {
        ClientRequest::ListGuilds => {
            let guilds = list_guilds(&state.api, user_id).await.ok()?;
            guilds
                .first()
                .cloned()
                .map(|guild| ServerEvent::GuildUpdated { guild })
        }
        ClientRequest::ListChannels { guild_id } => {
            let channels = list_channels(&state.api, user_id, guild_id).await.ok()?;
            channels
                .first()
                .cloned()
                .map(|channel| ServerEvent::ChannelUpdated { channel })
        }
        ClientRequest::SendMessage {
            channel_id,
            ciphertext_b64,
        } => send_message(&state.api, user_id, GuildId(1), channel_id, &ciphertext_b64)
            .await
            .ok(),
        ClientRequest::RequestLiveKitToken {
            guild_id,
            channel_id,
            can_publish_mic,
            can_publish_screen,
        } => request_livekit_token(
            &state.api,
            user_id,
            guild_id,
            channel_id,
            can_publish_mic,
            can_publish_screen,
        )
        .await
        .ok(),
        _ => Some(ServerEvent::Error(ApiError::new(
            ErrorCode::Validation,
            "request variant not yet handled in websocket transport",
        ))),
    }
}
