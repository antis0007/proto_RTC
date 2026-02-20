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
async fn fetch_pending_welcome_succeeds_without_consuming_on_read() {
    let (app, storage, user_id, guild_id, channel_id) = test_app().await;
    let welcome_bytes = b"welcome-payload";

    storage
        .insert_pending_welcome(
            GuildId(guild_id),
            ChannelId(channel_id),
            UserId(user_id),
            None,
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
    assert!(dto.consumed_at.is_none());
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
    assert_eq!(second_response.status(), StatusCode::OK);
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
    assert!(dto.consumed_at.is_none());
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
            None,
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
