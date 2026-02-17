use std::{net::SocketAddr, sync::Arc};

use axum::{
    body::Bytes,
    extract::{Path, Query, State, WebSocketUpgrade},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use livekit_integration::LiveKitConfig;
use serde::{Deserialize, Serialize};
use server_api::{
    list_channels, list_guilds, list_members, list_messages, send_message, ApiContext,
};
use shared::{
    domain::{ChannelId, ChannelKind, FileId, GuildId, UserId},
    error::{ApiError, ErrorCode},
    protocol::{AttachmentPayload, KeyPackageResponse, ServerEvent, UploadKeyPackageResponse},
};
use storage::Storage;
use tokio::sync::broadcast;
use tracing::{error, info};

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
    filename: Option<String>,
    mime_type: Option<String>,
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
struct FileDownloadQuery {
    user_id: i64,
}

#[derive(Debug, Deserialize)]
struct UploadKeyPackageQuery {
    user_id: i64,
    guild_id: i64,
}

#[derive(Debug, Deserialize)]
struct FetchKeyPackageQuery {
    guild_id: i64,
    user_id: i64,
}

#[derive(Debug, Deserialize)]
struct ListMessagesQuery {
    user_id: i64,
    limit: Option<u32>,
    before: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct JoinGuildRequest {
    user_id: i64,
    invite_code: String,
}

#[derive(Debug, Serialize)]
struct InviteResponse {
    invite_code: String,
}

#[derive(Debug, Deserialize)]
struct SendMessageRequest {
    user_id: i64,
    guild_id: i64,
    channel_id: i64,
    ciphertext_b64: String,
    #[serde(default)]
    attachment: Option<AttachmentPayload>,
}

const MAX_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_FILENAME_BYTES: usize = 180;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let settings = load_settings();
    let database_url = prepare_database_url(&settings.database_url)?;
    let storage = Storage::new(&database_url).await.map_err(|error| {
        error!(
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
    let app = build_router(Arc::new(state));

    let addr: SocketAddr = settings.server_bind.parse()?;
    info!(%addr, "server listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/login", post(login))
        .route("/guilds", get(http_list_guilds))
        .route("/guilds/:guild_id/channels", get(http_list_channels))
        .route("/guilds/:guild_id/members", get(http_list_members))
        .route("/channels/:channel_id/messages", get(http_list_messages))
        .route("/guilds/:guild_id/invites", post(http_create_invite))
        .route("/guilds/join", post(http_join_guild))
        .route("/messages", post(http_send_message))
        .route("/files/upload", post(upload_file))
        .route("/files/:file_id", get(download_file))
        .route("/mls/key_packages", post(upload_key_package))
        .route("/mls/key_packages", get(fetch_key_package))
        .route("/ws", get(ws_handler))
        .with_state(state)
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

    let guilds = state
        .api
        .storage
        .list_guilds_for_user(user_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?;

    if guilds.is_empty() {
        let guild_id = state
            .api
            .storage
            .create_guild("General", user_id)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError::new(ErrorCode::Internal, e.to_string())),
                )
            })?;

        state
            .api
            .storage
            .create_channel(guild_id, "general", ChannelKind::Text)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError::new(ErrorCode::Internal, e.to_string())),
                )
            })?;
    }

    Ok(Json(LoginResponse { user_id: user_id.0 }))
}

async fn upload_file(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FileUploadQuery>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    if body.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                ErrorCode::Validation,
                "attachment body cannot be empty",
            )),
        ));
    }
    if body.len() > MAX_ATTACHMENT_BYTES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ApiError::new(
                ErrorCode::Validation,
                format!("attachment exceeds {} bytes", MAX_ATTACHMENT_BYTES),
            )),
        ));
    }

    let filename = q
        .filename
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    if let Some(name) = filename {
        if name.len() > MAX_FILENAME_BYTES {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError::new(ErrorCode::Validation, "filename is too long")),
            ));
        }
        if name.contains('/') || name.contains('\\') {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError::new(
                    ErrorCode::Validation,
                    "filename must not contain path separators",
                )),
            ));
        }
    }

    state
        .api
        .storage
        .membership_status(GuildId(q.guild_id), UserId(q.user_id))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::FORBIDDEN,
                Json(ApiError::new(ErrorCode::Forbidden, "user is not a member")),
            )
        })?;

    let channel_guild = state
        .api
        .storage
        .guild_for_channel(ChannelId(q.channel_id))
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
                Json(ApiError::new(ErrorCode::NotFound, "channel not found")),
            )
        })?;
    if channel_guild != GuildId(q.guild_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                ErrorCode::Validation,
                "channel does not belong to guild",
            )),
        ));
    }

    let file_id = state
        .api
        .storage
        .store_file_ciphertext(
            UserId(q.user_id),
            GuildId(q.guild_id),
            ChannelId(q.channel_id),
            &body,
            q.mime_type
                .as_deref()
                .filter(|mime| !mime.trim().is_empty()),
            filename,
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?;
    let _ = state.events.send(ServerEvent::FileStored { file_id });
    Ok(Json(
        serde_json::json!({ "file_id": file_id.0, "size_bytes": body.len() }),
    ))
}

async fn download_file(
    State(state): State<Arc<AppState>>,
    Path(file_id): Path<i64>,
    Query(q): Query<FileDownloadQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    let file = state
        .api
        .storage
        .load_file(FileId(file_id))
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
    state
        .api
        .storage
        .membership_status(file.guild_id, UserId(q.user_id))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::FORBIDDEN,
                Json(ApiError::new(ErrorCode::Forbidden, "user is not a member")),
            )
        })?;

    let mut headers = HeaderMap::new();
    let content_type = file
        .mime_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    if let Some(filename) = file.filename {
        if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
            headers.insert(header::CONTENT_DISPOSITION, value);
        }
    }

    Ok((StatusCode::OK, headers, file.ciphertext))
}

async fn upload_key_package(
    State(state): State<Arc<AppState>>,
    Query(q): Query<UploadKeyPackageQuery>,
    body: Bytes,
) -> Result<Json<UploadKeyPackageResponse>, (StatusCode, Json<ApiError>)> {
    if body.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                ErrorCode::Validation,
                "key package body cannot be empty",
            )),
        ));
    }

    state
        .api
        .storage
        .membership_status(GuildId(q.guild_id), UserId(q.user_id))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::FORBIDDEN,
                Json(ApiError::new(ErrorCode::Forbidden, "user is not a member")),
            )
        })?;

    let key_package_id = state
        .api
        .storage
        .insert_key_package(GuildId(q.guild_id), UserId(q.user_id), &body)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?;

    Ok(Json(UploadKeyPackageResponse { key_package_id }))
}

async fn fetch_key_package(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FetchKeyPackageQuery>,
) -> Result<Json<KeyPackageResponse>, (StatusCode, Json<ApiError>)> {
    let (key_package_id, key_package_bytes) = state
        .api
        .storage
        .load_latest_key_package(GuildId(q.guild_id), UserId(q.user_id))
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
                Json(ApiError::new(ErrorCode::NotFound, "key package not found")),
            )
        })?;

    Ok(Json(KeyPackageResponse {
        key_package_id,
        guild_id: q.guild_id,
        user_id: q.user_id,
        key_package_b64: STANDARD.encode(key_package_bytes),
    }))
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

async fn http_list_members(
    State(state): State<Arc<AppState>>,
    Path(guild_id): Path<i64>,
    Query(q): Query<UserQuery>,
) -> Result<Json<Vec<shared::protocol::MemberSummary>>, (StatusCode, Json<ApiError>)> {
    let members = list_members(&state.api, UserId(q.user_id), GuildId(guild_id))
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(e)))?;
    Ok(Json(members))
}

async fn http_list_messages(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<i64>,
    Query(q): Query<ListMessagesQuery>,
) -> Result<Json<Vec<shared::protocol::MessagePayload>>, (StatusCode, Json<ApiError>)> {
    let limit = q.limit.unwrap_or(100).clamp(1, 100);
    let messages = list_messages(
        &state.api,
        UserId(q.user_id),
        ChannelId(channel_id),
        limit,
        q.before,
    )
    .await
    .map_err(|e| (StatusCode::BAD_REQUEST, Json(e)))?;
    Ok(Json(messages))
}

async fn http_create_invite(
    State(state): State<Arc<AppState>>,
    Path(guild_id): Path<i64>,
    Query(q): Query<UserQuery>,
) -> Result<Json<InviteResponse>, (StatusCode, Json<ApiError>)> {
    let guild_id = GuildId(guild_id);
    let user_id = UserId(q.user_id);
    state
        .api
        .storage
        .membership_status(guild_id, user_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::FORBIDDEN,
                Json(ApiError::new(ErrorCode::Forbidden, "user is not a member")),
            )
        })?;

    let payload = format!("guild:{}", guild_id.0);
    let invite_code = URL_SAFE_NO_PAD.encode(payload.as_bytes());
    Ok(Json(InviteResponse { invite_code }))
}

async fn http_join_guild(
    State(state): State<Arc<AppState>>,
    Json(req): Json<JoinGuildRequest>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let decoded = URL_SAFE_NO_PAD
        .decode(req.invite_code.as_bytes())
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(ApiError::new(ErrorCode::Validation, "invalid invite code")),
            )
        })?;
    let decoded_text = String::from_utf8(decoded).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(ErrorCode::Validation, "invalid invite code")),
        )
    })?;

    let guild_id = decoded_text
        .strip_prefix("guild:")
        .and_then(|id| id.parse::<i64>().ok())
        .map(GuildId)
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(ApiError::new(ErrorCode::Validation, "invalid invite code")),
            )
        })?;

    let joining_user_id = UserId(req.user_id);
    state
        .api
        .storage
        .add_membership(
            guild_id,
            joining_user_id,
            shared::domain::Role::Member,
            false,
            false,
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?;

    if let Ok(members) = list_members(&state.api, joining_user_id, guild_id).await {
        let _ = state
            .events
            .send(ServerEvent::GuildMembersUpdated { guild_id, members });
    }

    Ok(StatusCode::NO_CONTENT)
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
        req.attachment,
    )
    .await
    .map_err(|e| (StatusCode::BAD_REQUEST, Json(e)))?;
    let _ = state.events.send(event.clone());
    Ok(Json(event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    async fn test_app() -> (Router, i64, i64, i64) {
        let storage = Storage::new("sqlite::memory:").await.expect("db");
        let user = storage.create_user("alice").await.expect("user");
        let guild = storage.create_guild("general", user).await.expect("guild");
        let channel = storage
            .create_channel(guild, "general", ChannelKind::Text)
            .await
            .expect("channel");

        let api = ApiContext {
            storage,
            livekit: LiveKitConfig {
                api_key: "k".to_string(),
                api_secret: "s".to_string(),
                ttl_seconds: 60,
            },
        };
        let (events, _) = broadcast::channel(32);
        let app = build_router(Arc::new(AppState { api, events }));
        (app, user.0, guild.0, channel.0)
    }

    #[tokio::test]
    async fn file_upload_and_download_requires_membership() {
        let (app, user_id, guild_id, channel_id) = test_app().await;
        let upload = Request::post(format!(
            "/files/upload?user_id={user_id}&guild_id={guild_id}&channel_id={channel_id}&filename=test.bin"
        ))
        .body(Body::from("ciphertext"))
        .expect("request");
        let response = app.clone().oneshot(upload).await.expect("upload response");
        assert_eq!(response.status(), StatusCode::OK);

        let download = Request::get("/files/1?user_id=999")
            .body(Body::empty())
            .expect("request");
        let response = app.oneshot(download).await.expect("download response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
