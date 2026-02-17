use mls::{MlsGroupHandle, MlsIdentity};
use shared::domain::ChannelKind;
use storage::Storage;

#[tokio::test]
async fn onboarding_welcome_ciphertext_and_one_time_consumption_acceptance() {
    let storage = Storage::new("sqlite::memory:").await.expect("db");

    let alice = storage.create_user("lane3-alice").await.expect("alice");
    let bob = storage.create_user("lane3-bob").await.expect("bob");
    let guild = storage
        .create_guild("lane3-guild", alice)
        .await
        .expect("guild");
    storage
        .add_membership(guild, bob, shared::domain::Role::Member, false, false)
        .await
        .expect("add bob membership");
    let channel = storage
        .create_channel(guild, "general", ChannelKind::Text)
        .await
        .expect("channel");

    let mut alice_group = MlsGroupHandle::new(
        storage.clone(),
        guild,
        channel,
        MlsIdentity::new_with_name("alice").expect("alice identity"),
    )
    .await
    .expect("alice handle");
    alice_group
        .load_or_create_group()
        .await
        .expect("alice init group");

    let mut bob_group = MlsGroupHandle::new(
        storage.clone(),
        guild,
        channel,
        MlsIdentity::new_with_name("bob").expect("bob identity"),
    )
    .await
    .expect("bob handle");

    let bob_key_package = bob_group.key_package_bytes().expect("bob key package");
    let (_commit_for_existing, welcome_for_bob) = alice_group
        .add_member(&bob_key_package)
        .await
        .expect("add bob");
    storage
        .insert_pending_welcome(
            guild,
            channel,
            bob,
            &welcome_for_bob.expect("welcome exists for bob"),
        )
        .await
        .expect("store welcome");

    let consumed_once = storage
        .load_and_consume_pending_welcome(guild, channel, bob)
        .await
        .expect("load welcome once")
        .expect("welcome available");
    bob_group
        .join_group_from_welcome(&consumed_once)
        .await
        .expect("bob join from welcome");

    let plaintext = b"lane3 acceptance plaintext";
    let ciphertext = alice_group
        .encrypt_application(plaintext)
        .expect("alice encrypt");
    storage
        .insert_message_ciphertext(channel, alice, &ciphertext, None)
        .await
        .expect("store ciphertext");

    let stored = storage
        .list_channel_messages(channel, 1, None)
        .await
        .expect("list messages");
    assert_eq!(stored.len(), 1);
    assert_ne!(stored[0].ciphertext, plaintext);

    let decrypted = bob_group
        .decrypt_application(&stored[0].ciphertext)
        .await
        .expect("bob decrypts sender message");
    assert_eq!(decrypted, plaintext);

    let consumed_twice = storage
        .load_and_consume_pending_welcome(guild, channel, bob)
        .await
        .expect("second consume");
    assert!(consumed_twice.is_none());
}
