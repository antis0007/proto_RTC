use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

#[tokio::test]
async fn identity_persists_across_manager_restart_and_supports_commit_and_decrypt() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let db_path = std::env::temp_dir().join(format!("proto_rtc_mls_identity_{unique}.sqlite3"));
    let database_url = format!("sqlite://{}", db_path.display());

    let guild_id = GuildId(500);
    let channel_id = ChannelId(900);

    let alice = DurableMlsSessionManager::initialize(&database_url, 1, "device-alice")
        .await
        .expect("alice manager");
    let bob = DurableMlsSessionManager::initialize(&database_url, 2, "device-bob")
        .await
        .expect("bob manager");

    alice
        .open_or_create_group(guild_id, channel_id)
        .await
        .expect("alice group");
    bob.open_or_create_group(guild_id, channel_id)
        .await
        .expect("bob group slot");

    let bob_key_package = bob
        .key_package_bytes(guild_id)
        .await
        .expect("bob key package");
    let first_welcome = alice
        .add_member(channel_id, &bob_key_package)
        .await
        .expect("add bob");
    bob.join_from_welcome(guild_id, channel_id, &first_welcome.welcome_bytes)
        .await
        .expect("bob joins");

    let bob_after_restart = DurableMlsSessionManager::initialize(&database_url, 2, "device-bob")
        .await
        .expect("bob restart");
    bob_after_restart
        .open_or_create_group(guild_id, channel_id)
        .await
        .expect("open persisted group");

    let charlie = DurableMlsSessionManager::initialize(&database_url, 3, "device-charlie")
        .await
        .expect("charlie manager");
    charlie
        .open_or_create_group(guild_id, channel_id)
        .await
        .expect("charlie group slot");
    let charlie_key_package = charlie
        .key_package_bytes(guild_id)
        .await
        .expect("charlie key package");

    let charlie_add = alice
        .add_member(channel_id, &charlie_key_package)
        .await
        .expect("add charlie");

    let commit_merge_result = bob_after_restart
        .decrypt_application(channel_id, &charlie_add.commit_bytes)
        .await
        .expect("merge commit after restart");
    assert!(commit_merge_result.is_empty());

    let bob_ciphertext = bob_after_restart
        .encrypt_application(channel_id, b"bob after restart")
        .await
        .expect("bob encrypts with restored identity");
    let alice_plaintext = alice
        .decrypt_application(channel_id, &bob_ciphertext)
        .await
        .expect("alice decrypts bob message");
    assert_eq!(alice_plaintext, b"bob after restart");

    let _ = std::fs::remove_file(&db_path);
}
