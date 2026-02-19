use super::*;

async fn setup() -> (ApiContext, UserId, GuildId, ChannelId) {
    let storage = Storage::new("sqlite::memory:").await.expect("db");
    let user = storage.create_user("alice").await.expect("user");
    let guild = storage.create_guild("guild", user).await.expect("guild");
    let channel = storage
        .create_channel(guild, "voice", ChannelKind::Voice)
        .await
        .expect("channel");
    (
        ApiContext {
            storage,
            livekit: LiveKitConfig {
                api_key: "k".into(),
                api_secret: "s".into(),
                ttl_seconds: 60,
            },
        },
        user,
        guild,
        channel,
    )
}

#[tokio::test]
async fn banned_user_cannot_list_channels() {
    let (ctx, user, guild, _) = setup().await;
    ctx.storage
        .add_membership(guild, user, Role::Member, true, false)
        .await
        .expect("membership");
    let err = list_channels(&ctx, user, guild)
        .await
        .expect_err("should fail");
    assert!(matches!(err.code, ErrorCode::Forbidden));
}

#[tokio::test]
async fn muted_user_cannot_send_messages() {
    let (ctx, user, guild, channel) = setup().await;
    ctx.storage
        .add_membership(guild, user, Role::Member, false, true)
        .await
        .expect("membership");
    let err = send_message(&ctx, user, guild, channel, "b2theA==", None)
        .await
        .expect_err("should fail");
    assert!(matches!(err.code, ErrorCode::Forbidden));
}

#[tokio::test]
async fn list_members_includes_muted_flag() {
    let (ctx, user, guild, _) = setup().await;
    let bob = ctx.storage.create_user("bob").await.expect("user");
    ctx.storage
        .add_membership(guild, bob, Role::Member, false, true)
        .await
        .expect("membership");

    let members = list_members(&ctx, user, guild).await.expect("members");
    assert!(members.iter().any(|m| m.user_id == bob && m.muted));
}

#[tokio::test]
async fn send_and_list_message_with_attachment() {
    let (ctx, user, guild, channel) = setup().await;
    let file_id = ctx
        .storage
        .store_file_ciphertext(
            user,
            guild,
            channel,
            b"ciphertext",
            Some("application/octet-stream"),
            Some("blob.bin"),
        )
        .await
        .expect("file");

    let event = send_message(
        &ctx,
        user,
        guild,
        channel,
        "aGVsbG8=",
        Some(AttachmentPayload {
            file_id,
            filename: "blob.bin".to_string(),
            size_bytes: 10,
            mime_type: Some("application/octet-stream".to_string()),
        }),
    )
    .await
    .expect("send");

    let ServerEvent::MessageReceived { message } = event else {
        panic!("expected message event");
    };
    assert!(message.attachment.is_some());

    let listed = list_messages(&ctx, user, channel, 20, None)
        .await
        .expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].attachment.as_ref().expect("attachment").file_id,
        file_id
    );
}

#[tokio::test]
async fn request_livekit_token_returns_voice_channel_token() {
    let (ctx, user, guild, channel) = setup().await;
    let event = request_livekit_token(&ctx, user, guild, channel, true, false)
        .await
        .expect("token");

    let ServerEvent::LiveKitTokenIssued {
        guild_id,
        channel_id,
        room_name,
        token,
    } = event
    else {
        panic!("expected livekit token event");
    };

    assert_eq!(guild_id, guild);
    assert_eq!(channel_id, channel);
    assert_eq!(room_name, room_name_for_voice_channel(guild, channel));
    assert!(!token.is_empty());
}

#[tokio::test]
async fn send_message_rejects_channel_from_another_guild() {
    let (ctx, user, _guild, channel) = setup().await;
    let other_guild = ctx
        .storage
        .create_guild("other", user)
        .await
        .expect("guild");

    let err = send_message(&ctx, user, other_guild, channel, "aGVsbG8=", None)
        .await
        .expect_err("should fail");
    assert!(matches!(err.code, ErrorCode::Validation));
}
