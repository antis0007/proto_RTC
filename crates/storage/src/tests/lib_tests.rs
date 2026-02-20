use super::*;

#[tokio::test]
async fn stores_and_lists_guilds() {
    let storage = Storage::new("sqlite::memory:").await.expect("db");
    let user = storage.create_user("alice").await.expect("user");
    let guild = storage.create_guild("devs", user).await.expect("guild");
    let guilds = storage
        .list_guilds_for_user(user)
        .await
        .expect("guild list");
    assert_eq!(guilds.len(), 1);
    assert_eq!(guilds[0].0, guild);
}

#[tokio::test]
async fn health_check_succeeds_for_live_pool() {
    let storage = Storage::new("sqlite::memory:").await.expect("db");
    storage.health_check().await.expect("health check");
}

#[tokio::test]
async fn creates_database_file_when_missing() {
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let temp_root = std::env::temp_dir().join(format!("proto_rtc_storage_test_{suffix}"));
    let db_path = temp_root.join("nested").join("storage.db");
    let database_url = format!("sqlite://{}", db_path.to_string_lossy().replace('\\', "/"));

    let storage = Storage::new(&database_url).await.expect("db");
    drop(storage);

    assert!(
        db_path.exists(),
        "database file should exist: {}",
        db_path.display()
    );

    std::fs::remove_dir_all(temp_root).expect("cleanup");
}

#[tokio::test]
async fn inserts_message_ciphertext() {
    let storage = Storage::new("sqlite::memory:").await.expect("db");
    let user = storage.create_user("bob").await.expect("user");
    let guild = storage.create_guild("ops", user).await.expect("guild");
    let channel = storage
        .create_channel(guild, "general", ChannelKind::Text)
        .await
        .expect("channel");
    let message = storage
        .insert_message_ciphertext(channel, user, b"opaque", None)
        .await
        .expect("message");
    assert!(message.0 > 0);
}

#[tokio::test]
async fn paginates_channel_messages() {
    let storage = Storage::new("sqlite::memory:").await.expect("db");
    let user = storage.create_user("bob").await.expect("user");
    let guild = storage.create_guild("ops", user).await.expect("guild");
    let channel = storage
        .create_channel(guild, "general", ChannelKind::Text)
        .await
        .expect("channel");

    let first = storage
        .insert_message_ciphertext(channel, user, b"first", None)
        .await
        .expect("first");
    let second = storage
        .insert_message_ciphertext(channel, user, b"second", None)
        .await
        .expect("second");
    let _third = storage
        .insert_message_ciphertext(channel, user, b"third", None)
        .await
        .expect("third");

    let newest_two = storage
        .list_channel_messages(channel, 2, None)
        .await
        .expect("messages");
    assert_eq!(newest_two.len(), 2);
    assert_eq!(newest_two[0].message_id, second);

    let older = storage
        .list_channel_messages(channel, 2, Some(second.0))
        .await
        .expect("messages");
    assert_eq!(older.len(), 1);
    assert_eq!(older[0].message_id, first);
}

#[tokio::test]
async fn stores_latest_key_package_per_user_per_guild() {
    let storage = Storage::new("sqlite::memory:").await.expect("db");
    let user = storage.create_user("carol").await.expect("user");
    let guild = storage.create_guild("security", user).await.expect("guild");

    let first_id = storage
        .insert_key_package(guild, user, None, b"kp-1")
        .await
        .expect("insert key package");
    let second_id = storage
        .insert_key_package(guild, user, None, b"kp-2")
        .await
        .expect("insert key package");
    assert!(second_id > first_id);

    let latest = storage
        .load_latest_key_package(guild, user, None)
        .await
        .expect("latest")
        .expect("some latest");
    assert_eq!(latest.0, second_id);
    assert_eq!(latest.1, None);
    assert_eq!(latest.2, b"kp-2");
}

#[tokio::test]
async fn loads_latest_unconsumed_pending_welcome() {
    let storage = Storage::new("sqlite::memory:").await.expect("db");
    let user = storage.create_user("erin").await.expect("user");
    let guild = storage
        .create_guild("onboarding", user)
        .await
        .expect("guild");
    let channel = storage
        .create_channel(guild, "general", ChannelKind::Text)
        .await
        .expect("channel");

    storage
        .insert_pending_welcome(guild, channel, user, None, b"welcome-v1")
        .await
        .expect("insert welcome");
    storage
        .insert_pending_welcome(guild, channel, user, None, b"welcome-v2")
        .await
        .expect("insert welcome");

    let welcome = storage
        .load_and_consume_pending_welcome(guild, channel, user)
        .await
        .expect("load welcome")
        .expect("welcome exists");
    assert_eq!(welcome.welcome_bytes, b"welcome-v2");
    assert!(welcome.consumed_at <= Utc::now());
}

#[tokio::test]
async fn consumed_pending_welcome_is_not_returned_again() {
    let storage = Storage::new("sqlite::memory:").await.expect("db");
    let user = storage.create_user("frank").await.expect("user");
    let guild = storage
        .create_guild("onboarding", user)
        .await
        .expect("guild");
    let channel = storage
        .create_channel(guild, "general", ChannelKind::Text)
        .await
        .expect("channel");

    storage
        .insert_pending_welcome(guild, channel, user, None, b"single-use")
        .await
        .expect("insert welcome");

    let first = storage
        .load_and_consume_pending_welcome(guild, channel, user)
        .await
        .expect("first load");
    assert_eq!(
        first
            .as_ref()
            .map(|welcome| welcome.welcome_bytes.as_slice()),
        Some(b"single-use".as_ref())
    );

    let second = storage
        .load_and_consume_pending_welcome(guild, channel, user)
        .await
        .expect("second load");
    assert!(second.is_none());
}

#[tokio::test]
async fn pending_welcome_lookup_is_isolated_by_guild_channel_and_user() {
    let storage = Storage::new("sqlite::memory:").await.expect("db");

    let alice = storage.create_user("alice-isolation").await.expect("alice");
    let bob = storage.create_user("bob-isolation").await.expect("bob");

    let guild_a = storage
        .create_guild("guild-a", alice)
        .await
        .expect("guild a");
    storage
        .add_membership(guild_a, bob, Role::Member, false, false)
        .await
        .expect("membership");

    let guild_b = storage.create_guild("guild-b", bob).await.expect("guild b");
    storage
        .add_membership(guild_b, alice, Role::Member, false, false)
        .await
        .expect("membership");

    let channel_a1 = storage
        .create_channel(guild_a, "general", ChannelKind::Text)
        .await
        .expect("channel a1");
    let channel_a2 = storage
        .create_channel(guild_a, "random", ChannelKind::Text)
        .await
        .expect("channel a2");
    let channel_b1 = storage
        .create_channel(guild_b, "general", ChannelKind::Text)
        .await
        .expect("channel b1");

    storage
        .insert_pending_welcome(guild_a, channel_a1, alice, None, b"target")
        .await
        .expect("insert target");
    storage
        .insert_pending_welcome(guild_a, channel_a1, bob, None, b"other-user")
        .await
        .expect("insert other user");
    storage
        .insert_pending_welcome(guild_a, channel_a2, alice, None, b"other-channel")
        .await
        .expect("insert other channel");
    storage
        .insert_pending_welcome(guild_b, channel_b1, alice, None, b"other-guild")
        .await
        .expect("insert other guild");

    let welcome = storage
        .load_and_consume_pending_welcome(guild_a, channel_a1, alice)
        .await
        .expect("load target")
        .expect("target exists");
    assert_eq!(welcome.welcome_bytes, b"target");

    let other_user = storage
        .load_and_consume_pending_welcome(guild_a, channel_a1, bob)
        .await
        .expect("load other user")
        .expect("other user exists");
    assert_eq!(other_user.welcome_bytes, b"other-user");

    let other_channel = storage
        .load_and_consume_pending_welcome(guild_a, channel_a2, alice)
        .await
        .expect("load other channel")
        .expect("other channel exists");
    assert_eq!(other_channel.welcome_bytes, b"other-channel");

    let other_guild = storage
        .load_and_consume_pending_welcome(guild_b, channel_b1, alice)
        .await
        .expect("load other guild")
        .expect("other guild exists");
    assert_eq!(other_guild.welcome_bytes, b"other-guild");
}

#[tokio::test]
async fn consuming_pending_welcome_is_race_safe() {
    let storage = Storage::new("sqlite::memory:").await.expect("db");
    let user = storage.create_user("race-user").await.expect("user");
    let guild = storage
        .create_guild("race-guild", user)
        .await
        .expect("guild");
    let channel = storage
        .create_channel(guild, "race-channel", ChannelKind::Text)
        .await
        .expect("channel");

    storage
        .insert_pending_welcome(guild, channel, user, None, b"race-welcome")
        .await
        .expect("insert welcome");

    let storage_a = storage.clone();
    let storage_b = storage.clone();
    let (left, right) = tokio::join!(
        async move {
            storage_a
                .load_and_consume_pending_welcome(guild, channel, user)
                .await
                .expect("left consume")
        },
        async move {
            storage_b
                .load_and_consume_pending_welcome(guild, channel, user)
                .await
                .expect("right consume")
        }
    );

    let consumed = [left, right].into_iter().flatten().count();
    assert_eq!(consumed, 1, "exactly one fetch should consume the welcome");
}

#[tokio::test]
async fn mls_group_state_is_scoped_by_user_and_device_identity() {
    let storage = Storage::new("sqlite::memory:").await.expect("db");
    let guild = GuildId(7);
    let channel = ChannelId(11);

    let alice_snapshot = PersistedGroupSnapshot {
        schema_version: 1,
        group_state_blob: b"alice-group".to_vec(),
        key_material_blob: b"alice-keys".to_vec(),
    };
    let bob_snapshot = PersistedGroupSnapshot {
        schema_version: 1,
        group_state_blob: b"bob-group".to_vec(),
        key_material_blob: b"bob-keys".to_vec(),
    };

    storage
        .save_group_state(1001, "desktop", guild, channel, alice_snapshot.clone())
        .await
        .expect("save alice snapshot");
    storage
        .save_group_state(1002, "desktop", guild, channel, bob_snapshot.clone())
        .await
        .expect("save bob snapshot");

    let loaded_alice = storage
        .load_group_state(1001, "desktop", guild, channel)
        .await
        .expect("load alice")
        .expect("alice exists");
    let loaded_bob = storage
        .load_group_state(1002, "desktop", guild, channel)
        .await
        .expect("load bob")
        .expect("bob exists");

    assert_eq!(
        loaded_alice.group_state_blob,
        alice_snapshot.group_state_blob
    );
    assert_eq!(
        loaded_alice.key_material_blob,
        alice_snapshot.key_material_blob
    );
    assert_eq!(loaded_bob.group_state_blob, bob_snapshot.group_state_blob);
    assert_eq!(loaded_bob.key_material_blob, bob_snapshot.key_material_blob);
}

#[tokio::test]
async fn stores_message_attachment_metadata() {
    let storage = Storage::new("sqlite::memory:").await.expect("db");
    let user = storage.create_user("dave").await.expect("user");
    let guild = storage.create_guild("files", user).await.expect("guild");
    let channel = storage
        .create_channel(guild, "uploads", ChannelKind::Text)
        .await
        .expect("channel");

    let file_id = storage
        .store_file_ciphertext(
            user,
            guild,
            channel,
            b"encrypted-bytes",
            Some("text/plain"),
            Some("hello.txt"),
        )
        .await
        .expect("file");

    storage
        .insert_message_ciphertext(
            channel,
            user,
            b"see attachment",
            Some(&StoredAttachment {
                file_id,
                filename: "hello.txt".to_string(),
                size_bytes: 15,
                mime_type: Some("text/plain".to_string()),
            }),
        )
        .await
        .expect("message");

    let messages = storage
        .list_channel_messages(channel, 10, None)
        .await
        .expect("messages");
    let attachment = messages[0].attachment.as_ref().expect("attachment");
    assert_eq!(attachment.file_id, file_id);
    assert_eq!(attachment.filename, "hello.txt");
    assert_eq!(attachment.size_bytes, 15);
}
