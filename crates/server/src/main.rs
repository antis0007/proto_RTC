use std::{net::SocketAddr, sync::Arc};

use crate::api::{
    ensure_active_membership_in_channel, ensure_active_membership_in_guild, list_channels,
    list_guilds, list_members, list_messages, mls_bootstrap_request_route, mls_key_packages_route,
    mls_welcome_recovery_route, mls_welcome_route, request_livekit_token, send_message, ApiContext,
    KeyPackageResponse, MlsKeyPackageQuery, MlsWelcomeQuery, MlsWelcomeResponse,
    UploadKeyPackageResponse,
};
use crate::livekit::LiveKitConfig;
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State, WebSocketUpgrade},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use shared::{
    domain::{ChannelId, ChannelKind, DeviceId, FileId, GuildId, UserId},
    error::{ApiError, ErrorCode},
    protocol::{
        AttachmentPayload, DeviceLinkBundleFetchRequest, DeviceLinkBundleUploadRequest,
        DeviceLinkStartResponse, MlsBootstrapReason, ServerEvent,
    },
};
use storage::Storage;
use tokio::sync::broadcast;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::{error, info};

mod api;
mod app_state;
mod config;
mod livekit;
mod router;
mod routes;
mod ws;

use app_state::AppState;
use config::{load_settings, prepare_database_url};

#[derive(Debug, Deserialize)]
struct LoginRequest {
    username: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct LoginResponse {
    user_id: i64,
}

#[derive(Debug, Deserialize)]
struct RegisterDeviceRequest {
    device_name: String,
    device_public_identity: String,
}

#[derive(Debug, Deserialize)]
struct DeviceQuery {
    user_id: i64,
    device_id: i64,
}

#[derive(Debug, Deserialize)]
struct DeviceRegisterQuery {
    user_id: i64,
}

#[derive(Debug, Deserialize)]
struct DeviceLinkStartQuery {
    user_id: i64,
    target_device_id: i64,
}

#[derive(Debug, Deserialize)]
struct DeviceLinkCompleteQuery {
    user_id: i64,
    token_id: i64,
}

#[derive(Debug, Deserialize)]
struct DeviceLinkBundleFetchQuery {
    user_id: i64,
}

#[derive(Debug, Deserialize)]
struct DeviceLinkStartRequest {
    target_device_pubkey: String,
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
    #[serde(default)]
    target_device_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct MlsBootstrapRequestQuery {
    user_id: i64,
    guild_id: i64,
    channel_id: i64,
    #[serde(default)]
    target_user_id: Option<i64>,
    #[serde(default)]
    target_device_id: Option<i64>,
    #[serde(default)]
    reason: MlsBootstrapReason,
}

#[derive(Debug, Deserialize)]
struct RecoveryWelcomeQuery {
    user_id: i64,
    guild_id: i64,
    channel_id: i64,
    target_user_id: i64,
    #[serde(default)]
    target_device_id: Option<i64>,
}

const MAX_ATTACHMENT_BYTES: usize = 64 * 1024 * 1024; //64 MB
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
        mls_welcome_recovery_route(),
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
    let file_upload_router = Router::new()
        .route("/files/upload", post(upload_file))
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(MAX_ATTACHMENT_BYTES));

    Router::new()
        .route("/healthz", get(healthz))
        .route("/login", post(login))
        .route("/guilds", get(http_list_guilds))
        .route("/devices/register", post(register_device))
        .route("/devices/me", get(get_my_device))
        .route("/devices/link/start", post(start_device_link))
        .route("/devices/link/bundle", post(upload_device_link_bundle))
        .route("/devices/link/bundle/fetch", post(fetch_device_link_bundle))
        .route("/devices/link/complete", post(complete_device_link))
        .route("/users/:user_id/devices", get(list_user_devices))
        .route("/guilds/:guild_id/channels", get(http_list_channels))
        .route("/guilds/:guild_id/members", get(http_list_members))
        .route("/channels/:channel_id/messages", get(http_list_messages))
        .route("/guilds/:guild_id/invites", post(http_create_invite))
        .route("/guilds/join", post(http_join_guild))
        .route("/messages", post(http_send_message))
        .route("/livekit/token", post(http_request_livekit_token))
        .merge(file_upload_router) // <- add this
        .route("/files/:file_id", get(download_file))
        .route(mls_key_packages_route(), post(upload_key_package))
        .route(mls_key_packages_route(), get(fetch_key_package))
        .route(mls_welcome_route(), post(store_pending_welcome))
        .route(mls_welcome_route(), get(fetch_pending_welcome))
        .route(mls_welcome_recovery_route(), post(issue_recovery_welcome))
        .route(mls_bootstrap_request_route(), post(request_mls_bootstrap))
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
        let guild_name = format!("{}'s guild", req.username.trim());
        let guild_id = state
            .api
            .storage
            .create_guild(&guild_name, user_id)
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
        state
            .api
            .storage
            .create_channel(guild_id, "voice", ChannelKind::Voice)
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

async fn register_device(
    State(state): State<Arc<AppState>>,
    Query(q): Query<DeviceRegisterQuery>,
    Json(req): Json<RegisterDeviceRequest>,
) -> Result<Json<shared::domain::DeviceSummary>, (StatusCode, Json<ApiError>)> {
    let device = state
        .api
        .storage
        .register_device(
            UserId(q.user_id),
            &req.device_name,
            &req.device_public_identity,
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?;

    Ok(Json(shared::domain::DeviceSummary {
        device_id: device.device_id,
        user_id: device.user_id,
        device_name: device.device_name,
        device_public_identity: device.device_public_identity,
        is_revoked: device.is_revoked,
    }))
}

async fn get_my_device(
    State(state): State<Arc<AppState>>,
    Query(q): Query<DeviceQuery>,
) -> Result<Json<shared::domain::DeviceSummary>, (StatusCode, Json<ApiError>)> {
    let device = state
        .api
        .storage
        .get_device(UserId(q.user_id), DeviceId(q.device_id))
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
                Json(ApiError::new(ErrorCode::NotFound, "device not found")),
            )
        })?;

    Ok(Json(shared::domain::DeviceSummary {
        device_id: device.device_id,
        user_id: device.user_id,
        device_name: device.device_name,
        device_public_identity: device.device_public_identity,
        is_revoked: device.is_revoked,
    }))
}

async fn start_device_link(
    State(state): State<Arc<AppState>>,
    Query(q): Query<DeviceLinkStartQuery>,
    Json(req): Json<DeviceLinkStartRequest>,
) -> Result<Json<DeviceLinkStartResponse>, (StatusCode, Json<ApiError>)> {
    let expires_at = Utc::now() + chrono::Duration::minutes(3);
    let target_device = state
        .api
        .storage
        .get_device(UserId(q.user_id), DeviceId(q.target_device_id))
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
                    "target device not found",
                )),
            )
        })?;

    let token_id = state
        .api
        .storage
        .create_device_link_token(
            UserId(q.user_id),
            DeviceId(q.target_device_id),
            &req.target_device_pubkey,
            expires_at,
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?;

    let token = state
        .api
        .storage
        .load_device_link_token(UserId(q.user_id), token_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(
                    ErrorCode::Internal,
                    "link token missing after creation",
                )),
            )
        })?;

    let _ = target_device;

    Ok(Json(DeviceLinkStartResponse {
        token_id,
        token_secret: token.token_secret,
        expires_at,
    }))
}

async fn upload_device_link_bundle(
    State(state): State<Arc<AppState>>,
    Query(q): Query<UserQuery>,
    Json(req): Json<DeviceLinkBundleUploadRequest>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let token = state
        .api
        .storage
        .load_device_link_token(UserId(q.user_id), req.token_id)
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
                Json(ApiError::new(ErrorCode::NotFound, "link token not found")),
            )
        })?;

    if token.token_secret != req.token_secret
        || token.expires_at <= Utc::now()
        || token.consumed_at.is_some()
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                ErrorCode::Validation,
                "invalid or expired link token",
            )),
        ));
    }

    let bundle_json = serde_json::to_string(&req.bundle).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                ErrorCode::Validation,
                format!("invalid bundle payload: {e}"),
            )),
        )
    })?;

    state
        .api
        .storage
        .store_device_link_bundle(
            UserId(q.user_id),
            req.token_id,
            req.source_device_id,
            req.bundle.target_device_id,
            &bundle_json,
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

async fn fetch_device_link_bundle(
    State(state): State<Arc<AppState>>,
    Query(q): Query<DeviceLinkBundleFetchQuery>,
    Json(req): Json<DeviceLinkBundleFetchRequest>,
) -> Result<Json<shared::protocol::EncryptedChannelStateBundleV1>, (StatusCode, Json<ApiError>)> {
    let token = state
        .api
        .storage
        .load_device_link_token(UserId(q.user_id), req.token_id)
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
                Json(ApiError::new(ErrorCode::NotFound, "link token not found")),
            )
        })?;

    if token.token_secret != req.token_secret || token.expires_at <= Utc::now() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                ErrorCode::Validation,
                "invalid or expired link token",
            )),
        ));
    }

    if token.initiator_device_id != req.target_device_id {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiError::new(ErrorCode::Forbidden, "token/device mismatch")),
        ));
    }

    let bundle = state
        .api
        .storage
        .consume_device_link_bundle(UserId(q.user_id), req.token_id)
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
                    "bundle not found or already consumed",
                )),
            )
        })?;

    let decoded = serde_json::from_str(&bundle.bundle_json).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError::new(
                ErrorCode::Internal,
                format!("bundle decode failed: {e}"),
            )),
        )
    })?;

    Ok(Json(decoded))
}

async fn complete_device_link(
    State(state): State<Arc<AppState>>,
    Query(q): Query<DeviceLinkCompleteQuery>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let consumed = state
        .api
        .storage
        .consume_device_link_token(UserId(q.user_id), q.token_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?;

    if !consumed {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                ErrorCode::Validation,
                "invalid or expired link token",
            )),
        ));
    }

    Ok(StatusCode::NO_CONTENT)
}

async fn list_user_devices(
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<i64>,
    Query(q): Query<UserQuery>,
) -> Result<Json<Vec<shared::domain::LinkedDeviceSummary>>, (StatusCode, Json<ApiError>)> {
    if q.user_id != user_id {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiError::new(
                ErrorCode::Forbidden,
                "cannot list devices for another user",
            )),
        ));
    }
    let devices = state
        .api
        .storage
        .list_devices_for_user(UserId(user_id))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?;
    Ok(Json(devices))
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

    let guild_id = GuildId(q.guild_id);
    let user_id = UserId(q.user_id);
    info!(
        guild_id = guild_id.0,
        user_id = user_id.0,
        key_package_size = body.len(),
        "mls: upload key package request"
    );
    let key_package_id = state
        .api
        .storage
        .insert_key_package(guild_id, user_id, q.device_id.map(DeviceId), &body)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?;

    if let Ok(members) = list_members(&state.api, user_id, guild_id).await {
        let _ = state
            .events
            .send(ServerEvent::GuildMembersUpdated { guild_id, members });
    }

    info!(
        guild_id = guild_id.0,
        user_id = user_id.0,
        key_package_id,
        "mls: key package stored"
    );

    Ok(Json(UploadKeyPackageResponse { key_package_id }))
}

async fn fetch_key_package(
    State(state): State<Arc<AppState>>,
    Query(q): Query<MlsKeyPackageQuery>,
) -> Result<Json<KeyPackageResponse>, (StatusCode, Json<ApiError>)> {
    ensure_active_membership_in_guild(&state.api, UserId(q.user_id), GuildId(q.guild_id))
        .await
        .map_err(|error| (api_error_status(&error), Json(error)))?;

    let target_user_id = q.target_user_id.unwrap_or(q.user_id);

    let (key_package_id, key_package_device_id, key_package_bytes) = state
        .api
        .storage
        .load_latest_key_package(
            GuildId(q.guild_id),
            UserId(target_user_id),
            q.target_device_id.map(DeviceId),
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
                Json(ApiError::new(ErrorCode::NotFound, "key package not found")),
            )
        })?;

    Ok(Json(KeyPackageResponse {
        key_package_id,
        guild_id: q.guild_id,
        user_id: target_user_id,
        device_id: key_package_device_id,
        key_package_b64: STANDARD.encode(key_package_bytes),
    }))
}

async fn fetch_pending_welcome(
    State(state): State<Arc<AppState>>,
    Query(q): Query<MlsWelcomeQuery>,
) -> Result<Json<MlsWelcomeResponse>, (StatusCode, Json<ApiError>)> {
    info!(
        guild_id = q.guild_id,
        channel_id = q.channel_id,
        user_id = q.user_id,
        "mls: fetch pending welcome request"
    );
    ensure_active_membership_in_channel(
        &state.api,
        UserId(q.user_id),
        GuildId(q.guild_id),
        ChannelId(q.channel_id),
    )
    .await
    .map_err(|error| (api_error_status(&error), Json(error)))?;

    let pending_welcome = state
        .api
        .storage
        .load_pending_welcome(
            GuildId(q.guild_id),
            ChannelId(q.channel_id),
            UserId(q.target_user_id.unwrap_or(q.user_id)),
            q.target_device_id.map(DeviceId),
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?;

    let Some(pending_welcome) = pending_welcome else {
        info!(
            guild_id = q.guild_id,
            channel_id = q.channel_id,
            user_id = q.user_id,
            pending_welcome = false,
            "mls: fetch pending welcome result"
        );
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiError::new(
                ErrorCode::NotFound,
                "pending welcome not found",
            )),
        ));
    };

    info!(
        guild_id = q.guild_id,
        channel_id = q.channel_id,
        user_id = q.user_id,
        pending_welcome = true,
        welcome_size = pending_welcome.welcome_bytes.len(),
        "mls: fetch pending welcome result"
    );

    Ok(Json(MlsWelcomeResponse {
        user_id: q.target_user_id.unwrap_or(q.user_id),
        guild_id: q.guild_id,
        channel_id: q.channel_id,
        target_device_id: pending_welcome.target_device_id,
        welcome_b64: STANDARD.encode(pending_welcome.welcome_bytes),
        consumed_at: None,
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
            q.target_device_id.map(DeviceId),
            &body,
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?;

    let _ = state.events.send(ServerEvent::MlsWelcomeAvailable {
        guild_id: GuildId(q.guild_id),
        channel_id: ChannelId(q.channel_id),
        target_user_id: UserId(q.target_user_id),
        target_device_id: q.target_device_id.map(DeviceId),
    });

    info!(
        guild_id = q.guild_id,
        channel_id = q.channel_id,
        target_user_id = q.target_user_id,
        "mls: pending welcome stored and broadcast"
    );

    Ok(StatusCode::NO_CONTENT)
}

async fn request_mls_bootstrap(
    State(state): State<Arc<AppState>>,
    Query(q): Query<MlsBootstrapRequestQuery>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    ensure_active_membership_in_channel(
        &state.api,
        UserId(q.user_id),
        GuildId(q.guild_id),
        ChannelId(q.channel_id),
    )
    .await
    .map_err(|error| (api_error_status(&error), Json(error)))?;

    if let Some(target_user_id) = q.target_user_id {
        ensure_active_membership_in_channel(
            &state.api,
            UserId(target_user_id),
            GuildId(q.guild_id),
            ChannelId(q.channel_id),
        )
        .await
        .map_err(|error| (api_error_status(&error), Json(error)))?;
    }

    let _ = state.events.send(ServerEvent::MlsBootstrapRequested {
        guild_id: GuildId(q.guild_id),
        channel_id: ChannelId(q.channel_id),
        requesting_user_id: UserId(q.user_id),
        target_user_id: q.target_user_id.map(UserId),
        target_device_id: q.target_device_id.map(DeviceId),
        reason: q.reason,
    });

    info!(
        guild_id = q.guild_id,
        channel_id = q.channel_id,
        requesting_user_id = q.user_id,
        target_user_id = q.target_user_id,
        reason = ?q.reason,
        "mls: bootstrap requested"
    );

    Ok(StatusCode::NO_CONTENT)
}

async fn issue_recovery_welcome(
    State(state): State<Arc<AppState>>,
    Query(q): Query<RecoveryWelcomeQuery>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
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

    let latest_welcome = state
        .api
        .storage
        .load_latest_welcome_any_state(
            GuildId(q.guild_id),
            ChannelId(q.channel_id),
            UserId(q.target_user_id),
            q.target_device_id.map(DeviceId),
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
                StatusCode::CONFLICT,
                Json(ApiError::new(
                    ErrorCode::Validation,
                    "no welcome material available for recovery (server has no persisted welcome history for target)",
                )),
            )
        })?;

    state
        .api
        .storage
        .insert_pending_welcome(
            GuildId(q.guild_id),
            ChannelId(q.channel_id),
            UserId(q.target_user_id),
            q.target_device_id.map(DeviceId),
            &latest_welcome.welcome_bytes,
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(ErrorCode::Internal, e.to_string())),
            )
        })?;

    let _ = state.events.send(ServerEvent::MlsWelcomeAvailable {
        guild_id: GuildId(q.guild_id),
        channel_id: ChannelId(q.channel_id),
        target_user_id: UserId(q.target_user_id),
        target_device_id: q.target_device_id.map(DeviceId),
    });

    info!(
        guild_id = q.guild_id,
        channel_id = q.channel_id,
        requesting_user_id = q.user_id,
        target_user_id = q.target_user_id,
        "mls: recovery welcome generated/stored"
    );

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
    user_id: UserId,
) {
    use axum::extract::ws::Message;
    use futures::{SinkExt, StreamExt};

    let (mut sender, mut receiver) = socket.split();
    let mut events_rx = state.events.subscribe();

    let send_state = Arc::clone(&state);
    let send_task = tokio::spawn(async move {
        loop {
            let event = match events_rx.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            if !is_event_visible_to_user(&send_state, user_id, &event).await {
                continue;
            }
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

async fn is_event_visible_to_user(
    state: &Arc<AppState>,
    user_id: UserId,
    event: &ServerEvent,
) -> bool {
    let is_member = |guild_id: GuildId| async move {
        matches!(
            state.api.storage.membership_status(guild_id, user_id).await,
            Ok(Some((_role, banned, _muted))) if !banned
        )
    };

    match event {
        ServerEvent::GuildUpdated { guild } => is_member(guild.guild_id).await,
        ServerEvent::ChannelUpdated { channel } => is_member(channel.guild_id).await,
        ServerEvent::GuildMembersUpdated { guild_id, .. } => is_member(*guild_id).await,
        ServerEvent::MessageReceived { message } => {
            let guild_id = match state
                .api
                .storage
                .guild_for_channel(message.channel_id)
                .await
            {
                Ok(Some(guild_id)) => guild_id,
                _ => return false,
            };
            is_member(guild_id).await
        }
        ServerEvent::UserKicked {
            guild_id,
            target_user_id,
        }
        | ServerEvent::UserBanned {
            guild_id,
            target_user_id,
        }
        | ServerEvent::UserMuted {
            guild_id,
            target_user_id,
        } => *target_user_id == user_id || is_member(*guild_id).await,
        ServerEvent::LiveKitTokenIssued { guild_id, .. }
        | ServerEvent::MlsBootstrapRequested { guild_id, .. } => is_member(*guild_id).await,
        ServerEvent::MlsWelcomeAvailable {
            guild_id,
            target_user_id,
            ..
        } => *target_user_id == user_id && is_member(*guild_id).await,
        ServerEvent::FileStored { .. } | ServerEvent::Error(_) => true,
    }
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
    info!(
        guild_id = guild_id.0,
        user_id = joining_user_id.0,
        "guild: join with invite"
    );
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
#[path = "tests/main_tests.rs"]
mod tests;
