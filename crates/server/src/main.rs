use std::{net::SocketAddr, sync::Arc};

use crate::api::{
    ensure_active_membership_in_channel, ensure_active_membership_in_guild, list_channels,
    list_guilds, list_members, list_messages, mls_key_packages_route, mls_welcome_route,
    request_livekit_token, send_message, ApiContext, KeyPackageResponse, MlsKeyPackageQuery,
    MlsWelcomeQuery, MlsWelcomeResponse, UploadKeyPackageResponse,
};
use crate::livekit::LiveKitConfig;
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
use serde::{Deserialize, Serialize};
use shared::{
    domain::{ChannelId, ChannelKind, FileId, GuildId, UserId},
    error::{ApiError, ErrorCode},
    protocol::{AttachmentPayload, ServerEvent},
};
use storage::Storage;
use tokio::sync::broadcast;
use tracing::{error, info};

mod api;
mod config;
mod livekit;

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

#[derive(Debug, Deserialize, Serialize)]
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
struct LiveKitTokenQuery {
    user_id: i64,
    guild_id: i64,
    channel_id: i64,
    #[serde(default)]
    can_publish_mic: bool,
    #[serde(default)]
    can_publish_screen: bool,
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

#[derive(Debug, Deserialize)]
struct StorePendingWelcomeQuery {
    user_id: i64,
    guild_id: i64,
    channel_id: i64,
    target_user_id: i64,
}

const MAX_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_FILENAME_BYTES: usize = 180;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    info!("loading server settings");
    let settings = load_settings();
    info!(bind_addr = %settings.server_bind, "server settings parsed");
    if settings.server_bind.trim().is_empty() {
        error!("SERVER_BIND / bind_addr must not be empty");
        anyhow::bail!("SERVER_BIND / bind_addr must not be empty");
    }

    let database_url = prepare_database_url(&settings.database_url).map_err(|error| {
        error!(raw_database_url = %settings.database_url, %error, "database url preparation failed");
        error
    })?;
    info!(%database_url, "database url prepared");

    let storage = Storage::new(&database_url).await.map_err(|error| {
        error!(
            %database_url,
            %error,
            "database initialization/migrations failed; verify parent directory exists and permissions are correct"
        );
        error
    })?;
    info!("database initialized and migrations applied");

    storage.health_check().await.map_err(|error| {
        error!(%database_url, %error, "database health check failed after initialization");
        error
    })?;
    info!("database health check passed");

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

    let routes = [
        "/healthz",
        "/messages",
        "/channels/:channel_id/messages",
        "/files/upload",
        "/files/:file_id",
        mls_key_packages_route(),
        mls_welcome_route(),
    ];
    for route in routes {
        info!(%route, "route registered");
    }
    info!("router readiness checks complete");

    let addr: SocketAddr = settings.server_bind.parse().map_err(|error| {
        error!(server_bind = %settings.server_bind, %error, "invalid server bind address");
        error
    })?;

    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|error| {
        error!(%addr, %error, "failed to bind tcp listener");
        error
    })?;
    info!(%addr, "server listening");

    axum::serve(listener, app).await.map_err(|error| {
        error!(%addr, %error, "server terminated unexpectedly");
        error
    })?;
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
        .route("/livekit/token", post(http_request_livekit_token))
        .route("/files/upload", post(upload_file))
        .route("/files/:file_id", get(download_file))
        .route(mls_key_packages_route(), post(upload_key_package))
        .route(mls_key_packages_route(), get(fetch_key_package))
        .route(mls_welcome_route(), post(store_pending_welcome))
        .route(mls_welcome_route(), get(fetch_pending_welcome))
        .route("/ws", get(ws_handler))
        .with_state(state)
}

fn api_error_status(error: &ApiError) -> StatusCode {
    match error.code {
        ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
        ErrorCode::Forbidden => StatusCode::FORBIDDEN,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::Validation => StatusCode::BAD_REQUEST,
        ErrorCode::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn healthz(
    State(state): State<Arc<AppState>>,
) -> Result<&'static str, (StatusCode, Json<ApiError>)> {
    state.api.storage.health_check().await.map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError::new(
                ErrorCode::Internal,
                format!("storage health check failed: {error}"),
            )),
        )
    })?;
    Ok("ok")
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

    ensure_active_membership_in_channel(
        &state.api,
        UserId(q.user_id),
        GuildId(q.guild_id),
        ChannelId(q.channel_id),
    )
    .await
    .map_err(|error| (api_error_status(&error), Json(error)))?;

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
    ensure_active_membership_in_guild(&state.api, UserId(q.user_id), file.guild_id)
        .await
        .map_err(|error| (api_error_status(&error), Json(error)))?;

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
    Query(q): Query<MlsKeyPackageQuery>,
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

    ensure_active_membership_in_guild(&state.api, UserId(q.user_id), GuildId(q.guild_id))
        .await
        .map_err(|error| (api_error_status(&error), Json(error)))?;

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
    Query(q): Query<MlsKeyPackageQuery>,
) -> Result<Json<KeyPackageResponse>, (StatusCode, Json<ApiError>)> {
    ensure_active_membership_in_guild(&state.api, UserId(q.user_id), GuildId(q.guild_id))
        .await
        .map_err(|error| (api_error_status(&error), Json(error)))?;

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

async fn fetch_pending_welcome(
    State(state): State<Arc<AppState>>,
    Query(q): Query<MlsWelcomeQuery>,
) -> Result<Json<MlsWelcomeResponse>, (StatusCode, Json<ApiError>)> {
    ensure_active_membership_in_channel(
        &state.api,
        UserId(q.user_id),
        GuildId(q.guild_id),
        ChannelId(q.channel_id),
    )
    .await
    .map_err(|error| (api_error_status(&error), Json(error)))?;

    let consumed_welcome = state
        .api
        .storage
        .load_and_consume_pending_welcome(
            GuildId(q.guild_id),
            ChannelId(q.channel_id),
            UserId(q.user_id),
        )
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
                Json(ApiError::new(
                    ErrorCode::NotFound,
                    "pending welcome not found",
                )),
            )
        })?;

    Ok(Json(MlsWelcomeResponse {
        user_id: q.user_id,
        guild_id: q.guild_id,
        channel_id: q.channel_id,
        welcome_b64: STANDARD.encode(consumed_welcome.welcome_bytes),
        consumed_at: consumed_welcome.consumed_at,
    }))
}

async fn store_pending_welcome(
    State(state): State<Arc<AppState>>,
    Query(q): Query<StorePendingWelcomeQuery>,
    body: Bytes,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    if body.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                ErrorCode::Validation,
                "pending welcome body cannot be empty",
            )),
        ));
    }

    ensure_active_membership_in_channel(
        &state.api,
        UserId(q.user_id),
        GuildId(q.guild_id),
        ChannelId(q.channel_id),
    )
    .await
    .map_err(|error| (api_error_status(&error), Json(error)))?;

    ensure_active_membership_in_channel(
        &state.api,
        UserId(q.target_user_id),
        GuildId(q.guild_id),
        ChannelId(q.channel_id),
    )
    .await
    .map_err(|error| (api_error_status(&error), Json(error)))?;

    state
        .api
        .storage
        .insert_pending_welcome(
            GuildId(q.guild_id),
            ChannelId(q.channel_id),
            UserId(q.target_user_id),
            &body,
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?;

    Ok(StatusCode::NO_CONTENT)
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
        .map_err(|error| (api_error_status(&error), Json(error)))?;
    Ok(Json(guilds))
}

async fn http_list_channels(
    State(state): State<Arc<AppState>>,
    Path(guild_id): Path<i64>,
    Query(q): Query<UserQuery>,
) -> Result<Json<Vec<shared::protocol::ChannelSummary>>, (StatusCode, Json<ApiError>)> {
    let channels = list_channels(&state.api, UserId(q.user_id), GuildId(guild_id))
        .await
        .map_err(|error| (api_error_status(&error), Json(error)))?;
    Ok(Json(channels))
}

async fn http_list_members(
    State(state): State<Arc<AppState>>,
    Path(guild_id): Path<i64>,
    Query(q): Query<UserQuery>,
) -> Result<Json<Vec<shared::protocol::MemberSummary>>, (StatusCode, Json<ApiError>)> {
    let members = list_members(&state.api, UserId(q.user_id), GuildId(guild_id))
        .await
        .map_err(|error| (api_error_status(&error), Json(error)))?;
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
    .map_err(|error| (api_error_status(&error), Json(error)))?;
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

async fn http_request_livekit_token(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LiveKitTokenQuery>,
) -> Result<Json<ServerEvent>, (StatusCode, Json<ApiError>)> {
    let event = request_livekit_token(
        &state.api,
        UserId(q.user_id),
        GuildId(q.guild_id),
        ChannelId(q.channel_id),
        q.can_publish_mic,
        q.can_publish_screen,
    )
    .await
    .map_err(|error| (api_error_status(&error), Json(error)))?;
    Ok(Json(event))
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
    .map_err(|error| (api_error_status(&error), Json(error)))?;
    let _ = state.events.send(event.clone());
    Ok(Json(event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body, body::Body, http::Request};
    use tower::ServiceExt;

    async fn test_app() -> (Router, Storage, i64, i64, i64) {
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
        let app = build_router(Arc::new(AppState {
            api: api.clone(),
            events,
        }));
        (app, api.storage, user.0, guild.0, channel.0)
    }

    #[tokio::test]
    async fn healthz_reports_ok_when_storage_is_ready() {
        let (app, _storage, _user_id, _guild_id, _channel_id) = test_app().await;
        let request = Request::get("/healthz")
            .body(Body::empty())
            .expect("request");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        let body = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(body.as_ref(), b"ok");
    }

    #[tokio::test]
    async fn login_and_guild_channel_list_routes_work() {
        let (app, _storage, _user_id, _guild_id, _channel_id) = test_app().await;

        let login_request = Request::post("/login")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({ "username": "route-user" }).to_string(),
            ))
            .expect("request");
        let login_response = app.clone().oneshot(login_request).await.expect("response");
        assert_eq!(login_response.status(), StatusCode::OK);
        let login_body = body::to_bytes(login_response.into_body(), usize::MAX)
            .await
            .expect("body");
        let dto: LoginResponse = serde_json::from_slice(&login_body).expect("json");

        let guilds_request = Request::get(format!("/guilds?user_id={}", dto.user_id))
            .body(Body::empty())
            .expect("request");
        let guilds_response = app.clone().oneshot(guilds_request).await.expect("response");
        assert_eq!(guilds_response.status(), StatusCode::OK);
        let guilds_body = body::to_bytes(guilds_response.into_body(), usize::MAX)
            .await
            .expect("body");
        let guilds: Vec<shared::protocol::GuildSummary> =
            serde_json::from_slice(&guilds_body).expect("json");
        assert!(!guilds.is_empty());

        let channels_request = Request::get(format!(
            "/guilds/{}/channels?user_id={}",
            guilds[0].guild_id.0, dto.user_id
        ))
        .body(Body::empty())
        .expect("request");
        let channels_response = app.oneshot(channels_request).await.expect("response");
        assert_eq!(channels_response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn file_upload_and_download_requires_membership() {
        let (app, storage, user_id, guild_id, channel_id) = test_app().await;
        let upload = Request::post(format!(
            "/files/upload?user_id={user_id}&guild_id={guild_id}&channel_id={channel_id}&filename=test.bin"
        ))
        .body(Body::from("ciphertext"))
        .expect("request");
        let response = app.clone().oneshot(upload).await.expect("upload response");
        assert_eq!(response.status(), StatusCode::OK);

        let authorized_download = Request::get(format!("/files/1?user_id={user_id}"))
            .body(Body::empty())
            .expect("request");
        let authorized = app
            .clone()
            .oneshot(authorized_download)
            .await
            .expect("authorized download");
        assert_eq!(authorized.status(), StatusCode::OK);

        let outsider = storage.create_user("outsider-file").await.expect("user");
        let unauthorized_upload = Request::post(format!(
            "/files/upload?user_id={}&guild_id={guild_id}&channel_id={channel_id}&filename=test.bin",
            outsider.0
        ))
        .body(Body::from("ciphertext"))
        .expect("request");
        let unauthorized_upload_response = app
            .clone()
            .oneshot(unauthorized_upload)
            .await
            .expect("unauthorized upload response");
        assert_eq!(unauthorized_upload_response.status(), StatusCode::FORBIDDEN);

        let unauthorized_download = Request::get(format!("/files/1?user_id={}", outsider.0))
            .body(Body::empty())
            .expect("request");
        let response = app
            .oneshot(unauthorized_download)
            .await
            .expect("download response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn livekit_token_route_mints_token_for_voice_channel() {
        let (app, storage, user_id, guild_id, _channel_id) = test_app().await;
        let voice_channel = storage
            .create_channel(GuildId(guild_id), "voice", ChannelKind::Voice)
            .await
            .expect("voice channel");

        let request = Request::post(format!(
            "/livekit/token?user_id={user_id}&guild_id={guild_id}&channel_id={}&can_publish_mic=true",
            voice_channel.0
        ))
        .body(Body::empty())
        .expect("request");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn send_and_fetch_messages_enforce_membership_and_channel_scope() {
        let (app, storage, user_id, guild_id, channel_id) = test_app().await;

        let send_request = Request::post("/messages")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "user_id": user_id,
                    "guild_id": guild_id,
                    "channel_id": channel_id,
                    "ciphertext_b64": "aGVsbG8=",
                })
                .to_string(),
            ))
            .expect("request");
        let send_response = app.clone().oneshot(send_request).await.expect("response");
        assert_eq!(send_response.status(), StatusCode::OK);

        let list_request = Request::get(format!(
            "/channels/{channel_id}/messages?user_id={user_id}&limit=10"
        ))
        .body(Body::empty())
        .expect("request");
        let list_response = app.clone().oneshot(list_request).await.expect("response");
        assert_eq!(list_response.status(), StatusCode::OK);

        let outsider = storage.create_user("outsider").await.expect("user");
        let outsider_send = Request::post("/messages")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "user_id": outsider.0,
                    "guild_id": guild_id,
                    "channel_id": channel_id,
                    "ciphertext_b64": "aGVsbG8=",
                })
                .to_string(),
            ))
            .expect("request");
        let outsider_send_response = app.clone().oneshot(outsider_send).await.expect("response");
        assert_eq!(outsider_send_response.status(), StatusCode::FORBIDDEN);

        let outsider_list = Request::get(format!(
            "/channels/{channel_id}/messages?user_id={}&limit=10",
            outsider.0
        ))
        .body(Body::empty())
        .expect("request");
        let outsider_list_response = app.oneshot(outsider_list).await.expect("response");
        assert_eq!(outsider_list_response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn key_package_endpoints_require_active_membership() {
        let (app, storage, user_id, guild_id, _channel_id) = test_app().await;

        let upload = Request::post(format!(
            "/mls/key_packages?user_id={user_id}&guild_id={guild_id}"
        ))
        .body(Body::from("kp"))
        .expect("request");
        let upload_response = app.clone().oneshot(upload).await.expect("response");
        assert_eq!(upload_response.status(), StatusCode::OK);

        let fetch = Request::get(format!(
            "/mls/key_packages?user_id={user_id}&guild_id={guild_id}"
        ))
        .body(Body::empty())
        .expect("request");
        let fetch_response = app.clone().oneshot(fetch).await.expect("response");
        assert_eq!(fetch_response.status(), StatusCode::OK);

        storage
            .add_membership(
                GuildId(guild_id),
                UserId(user_id),
                shared::domain::Role::Member,
                true,
                false,
            )
            .await
            .expect("ban user");

        let banned_upload = Request::post(format!(
            "/mls/key_packages?user_id={user_id}&guild_id={guild_id}"
        ))
        .body(Body::from("kp2"))
        .expect("request");
        let banned_upload_response = app.clone().oneshot(banned_upload).await.expect("response");
        assert_eq!(banned_upload_response.status(), StatusCode::FORBIDDEN);

        let banned_fetch = Request::get(format!(
            "/mls/key_packages?user_id={user_id}&guild_id={guild_id}"
        ))
        .body(Body::empty())
        .expect("request");
        let banned_fetch_response = app.oneshot(banned_fetch).await.expect("response");
        assert_eq!(banned_fetch_response.status(), StatusCode::FORBIDDEN);
    }
    #[tokio::test]
    async fn fetch_pending_welcome_succeeds_and_consumes_on_read() {
        let (app, storage, user_id, guild_id, channel_id) = test_app().await;
        let welcome_bytes = b"welcome-payload";

        storage
            .insert_pending_welcome(
                GuildId(guild_id),
                ChannelId(channel_id),
                UserId(user_id),
                welcome_bytes,
            )
            .await
            .expect("insert welcome");

        let request = Request::get(format!(
            "/mls/welcome?user_id={user_id}&guild_id={guild_id}&channel_id={channel_id}"
        ))
        .body(Body::empty())
        .expect("request");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        let body = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let dto: MlsWelcomeResponse = serde_json::from_slice(&body).expect("json");
        assert_eq!(dto.user_id, user_id);
        assert_eq!(dto.guild_id, guild_id);
        assert_eq!(dto.channel_id, channel_id);
        assert!(dto.consumed_at.timestamp() > 0);
        assert_eq!(
            STANDARD.decode(dto.welcome_b64).expect("base64"),
            welcome_bytes
        );

        let second_request = Request::get(format!(
            "/mls/welcome?user_id={user_id}&guild_id={guild_id}&channel_id={channel_id}"
        ))
        .body(Body::empty())
        .expect("request");
        let second_response = app
            .clone()
            .oneshot(second_request)
            .await
            .expect("second response");
        assert_eq!(second_response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn store_pending_welcome_allows_target_to_fetch_and_join_later() {
        let (app, storage, user_id, guild_id, channel_id) = test_app().await;
        let target = storage.create_user("bob").await.expect("user");
        storage
            .add_membership(
                GuildId(guild_id),
                target,
                shared::domain::Role::Member,
                false,
                false,
            )
            .await
            .expect("membership");

        let store_request = Request::post(format!(
            "/mls/welcome?user_id={user_id}&guild_id={guild_id}&channel_id={channel_id}&target_user_id={}",
            target.0
        ))
        .body(Body::from("welcome-later"))
        .expect("request");
        let store_response = app.clone().oneshot(store_request).await.expect("response");
        assert_eq!(store_response.status(), StatusCode::NO_CONTENT);

        let fetch_request = Request::get(format!(
            "/mls/welcome?user_id={}&guild_id={guild_id}&channel_id={channel_id}",
            target.0
        ))
        .body(Body::empty())
        .expect("request");
        let fetch_response = app.clone().oneshot(fetch_request).await.expect("response");
        assert_eq!(fetch_response.status(), StatusCode::OK);

        let body = body::to_bytes(fetch_response.into_body(), usize::MAX)
            .await
            .expect("body");
        let dto: MlsWelcomeResponse = serde_json::from_slice(&body).expect("json");
        assert!(dto.consumed_at.timestamp() > 0);
        assert_eq!(
            STANDARD.decode(dto.welcome_b64).expect("base64"),
            b"welcome-later"
        );
    }

    #[tokio::test]
    async fn fetch_pending_welcome_rejects_non_member_and_does_not_consume() {
        let (app, storage, user_id, guild_id, channel_id) = test_app().await;
        storage
            .insert_pending_welcome(
                GuildId(guild_id),
                ChannelId(channel_id),
                UserId(user_id),
                b"single-use",
            )
            .await
            .expect("insert welcome");

        let outsider = storage.create_user("mallory").await.expect("user");
        let unauthorized_request = Request::get(format!(
            "/mls/welcome?user_id={}&guild_id={guild_id}&channel_id={channel_id}",
            outsider.0
        ))
        .body(Body::empty())
        .expect("request");
        let unauthorized_response = app
            .clone()
            .oneshot(unauthorized_request)
            .await
            .expect("response");
        assert_eq!(unauthorized_response.status(), StatusCode::FORBIDDEN);

        let authorized_request = Request::get(format!(
            "/mls/welcome?user_id={user_id}&guild_id={guild_id}&channel_id={channel_id}"
        ))
        .body(Body::empty())
        .expect("request");
        let authorized_response = app
            .oneshot(authorized_request)
            .await
            .expect("authorized response");
        assert_eq!(authorized_response.status(), StatusCode::OK);
    }
}
