use super::*;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::post,
    Json, Router,
};
use tokio::{net::TcpListener, sync::oneshot};

#[derive(Clone)]
struct ServerState {
    tx: Arc<Mutex<Option<oneshot::Sender<SendMessageHttpRequest>>>>,
}

struct TestMlsSessionManager {
    encrypt_ciphertext: Vec<u8>,
    decrypt_plaintext: Vec<u8>,
    add_member_commit: Vec<u8>,
    add_member_welcome: Vec<u8>,
    fail_with: Option<String>,
    exported_secret: Vec<u8>,
    joined_welcomes: Arc<Mutex<Vec<Vec<u8>>>>,
    decrypted_ciphertexts: Arc<Mutex<Vec<Vec<u8>>>>,
    has_persisted_group_state: bool,
    open_or_create_calls: Arc<Mutex<u32>>,
}

impl TestMlsSessionManager {
    fn ok(encrypt_ciphertext: Vec<u8>, decrypt_plaintext: Vec<u8>) -> Self {
        Self {
            encrypt_ciphertext,
            decrypt_plaintext,
            add_member_commit: b"commit-generated".to_vec(),
            add_member_welcome: b"welcome-generated".to_vec(),
            fail_with: None,
            exported_secret: b"mls-export-secret".to_vec(),
            joined_welcomes: Arc::new(Mutex::new(Vec::new())),
            decrypted_ciphertexts: Arc::new(Mutex::new(Vec::new())),
            has_persisted_group_state: false,
            open_or_create_calls: Arc::new(Mutex::new(0)),
        }
    }

    fn failing(err: impl Into<String>) -> Self {
        Self {
            encrypt_ciphertext: Vec::new(),
            decrypt_plaintext: Vec::new(),
            add_member_commit: Vec::new(),
            add_member_welcome: Vec::new(),
            fail_with: Some(err.into()),
            exported_secret: Vec::new(),
            joined_welcomes: Arc::new(Mutex::new(Vec::new())),
            decrypted_ciphertexts: Arc::new(Mutex::new(Vec::new())),
            has_persisted_group_state: false,
            open_or_create_calls: Arc::new(Mutex::new(0)),
        }
    }

    fn with_exported_secret(secret: Vec<u8>) -> Self {
        let mut manager = Self::ok(Vec::new(), Vec::new());
        manager.exported_secret = secret;
        manager
    }
    fn with_persisted_group_state(mut self, has_persisted_group_state: bool) -> Self {
        self.has_persisted_group_state = has_persisted_group_state;
        self
    }
}

#[async_trait]
impl MlsSessionManager for TestMlsSessionManager {
    async fn key_package_bytes(&self, _guild_id: GuildId) -> Result<Vec<u8>> {
        if let Some(err) = &self.fail_with {
            return Err(anyhow!(err.clone()));
        }
        Ok(b"test-key-package".to_vec())
    }

    async fn has_persisted_group_state(
        &self,
        _guild_id: GuildId,
        _channel_id: ChannelId,
    ) -> Result<bool> {
        if let Some(err) = &self.fail_with {
            return Err(anyhow!(err.clone()));
        }
        Ok(self.has_persisted_group_state)
    }

    async fn open_or_create_group(&self, _guild_id: GuildId, _channel_id: ChannelId) -> Result<()> {
        if let Some(err) = &self.fail_with {
            return Err(anyhow!(err.clone()));
        }

        let mut calls = self.open_or_create_calls.lock().await;
        *calls += 1;
        Ok(())
    }

    async fn reset_channel_group_state(
        &self,
        _guild_id: GuildId,
        _channel_id: ChannelId,
    ) -> Result<bool> {
        if let Some(err) = &self.fail_with {
            return Err(anyhow!(err.clone()));
        }

        // Default test behavior: nothing to reset.
        // Return true in specific tests if you later want to simulate a reset.
        Ok(false)
    }

    async fn encrypt_application(
        &self,
        _channel_id: ChannelId,
        plaintext: &[u8],
    ) -> Result<Vec<u8>> {
        if let Some(err) = &self.fail_with {
            return Err(anyhow!(err.clone()));
        }
        if plaintext.is_empty() {
            return Err(anyhow!("plaintext must not be empty"));
        }
        Ok(self.encrypt_ciphertext.clone())
    }

    async fn decrypt_application(
        &self,
        _channel_id: ChannelId,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>> {
        if let Some(err) = &self.fail_with {
            return Err(anyhow!(err.clone()));
        }

        self.decrypted_ciphertexts
            .lock()
            .await
            .push(ciphertext.to_vec());

        // Simulate non-application messages (e.g. commit/proposal) by returning empty plaintext.
        if ciphertext == self.add_member_commit.as_slice() {
            return Ok(Vec::new());
        }

        Ok(self.decrypt_plaintext.clone())
    }

    async fn add_member(
        &self,
        _channel_id: ChannelId,
        _key_package_bytes: &[u8],
    ) -> Result<MlsAddMemberOutcome> {
        if let Some(err) = &self.fail_with {
            return Err(anyhow!(err.clone()));
        }

        Ok(MlsAddMemberOutcome {
            commit_bytes: self.add_member_commit.clone(),
            welcome_bytes: self.add_member_welcome.clone(),
        })
    }

    async fn group_contains_key_package_identity(
        &self,
        _channel_id: ChannelId,
        _key_package_bytes: &[u8],
    ) -> Result<bool> {
        if let Some(err) = &self.fail_with {
            return Err(anyhow!(err.clone()));
        }
        Ok(false)
    }

    async fn join_from_welcome(
        &self,
        _guild_id: GuildId,
        _channel_id: ChannelId,
        welcome_bytes: &[u8],
    ) -> Result<()> {
        if let Some(err) = &self.fail_with {
            return Err(anyhow!(err.clone()));
        }

        self.joined_welcomes
            .lock()
            .await
            .push(welcome_bytes.to_vec());

        Ok(())
    }

    async fn export_secret(
        &self,
        _channel_id: ChannelId,
        _label: &str,
        _len: usize,
    ) -> Result<Vec<u8>> {
        if let Some(err) = &self.fail_with {
            return Err(anyhow!(err.clone()));
        }
        Ok(self.exported_secret.clone())
    }
}


async fn handle_send_message(
    State(state): State<ServerState>,
    Json(payload): Json<SendMessageHttpRequest>,
) {
    if let Some(tx) = state.tx.lock().await.take() {
        let _ = tx.send(payload);
    }
}

async fn spawn_message_server() -> Result<(String, oneshot::Receiver<SendMessageHttpRequest>)> {
    std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let (tx, rx) = oneshot::channel();
    let state = ServerState {
        tx: Arc::new(Mutex::new(Some(tx))),
    };
    let app = Router::new()
        .route("/messages", post(handle_send_message))
        .with_state(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{addr}"), rx))
}

#[tokio::test]
async fn send_message_uses_mls_ciphertext_payload() {
    let (server_url, payload_rx) = spawn_message_server().await.expect("spawn server");
    let client = RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::ok(
            b"mls-ciphertext".to_vec(),
            Vec::new(),
        )),
    );

    {
        let mut inner = client.inner.lock().await;
        inner.server_url = Some(server_url);
        inner.user_id = Some(7);
        inner.selected_guild = Some(GuildId(11));
        inner.selected_channel = Some(ChannelId(13));
        inner
            .initialized_mls_channels
            .insert((GuildId(11), ChannelId(13)));
    }

    client
        .send_message("plaintext-message")
        .await
        .expect("send");

    let payload = payload_rx.await.expect("payload");
    let plaintext_b64 = STANDARD.encode("plaintext-message".as_bytes());
    assert_ne!(payload.ciphertext_b64, plaintext_b64);
    assert_eq!(
        payload.ciphertext_b64,
        STANDARD.encode("mls-ciphertext".as_bytes())
    );
}

#[tokio::test]
async fn send_message_requires_active_mls_state() {
    let (server_url, _payload_rx) = spawn_message_server().await.expect("spawn server");
    let client = RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::failing("group not initialized")),
    );

    {
        let mut inner = client.inner.lock().await;
        inner.server_url = Some(server_url);
        inner.user_id = Some(7);
        inner.selected_guild = Some(GuildId(11));
        inner.selected_channel = Some(ChannelId(13));
    }

    let err = client
        .send_message("plaintext-message")
        .await
        .expect_err("must fail");
    let err_text = err.to_string();
    assert!(
        err_text.contains("uninitialized") || err_text.contains("failed to fetch guild members"),
        "unexpected error: {err_text}"
    );
}

#[tokio::test]
async fn send_message_restores_persisted_mls_state_automatically() {
    let (server_url, payload_rx) = spawn_message_server().await.expect("spawn server");
    let manager = TestMlsSessionManager::ok(b"mls-ciphertext".to_vec(), Vec::new())
        .with_persisted_group_state(true);
    let client = RealtimeClient::new_with_mls_session_manager(PassthroughCrypto, Arc::new(manager));

    {
        let mut inner = client.inner.lock().await;
        inner.server_url = Some(server_url);
        inner.user_id = Some(7);
        inner.selected_guild = Some(GuildId(11));
        inner.selected_channel = Some(ChannelId(13));
    }

    client
        .send_message("plaintext-message")
        .await
        .expect("send");

    let payload = payload_rx.await.expect("payload");
    assert_eq!(
        payload.ciphertext_b64,
        STANDARD.encode("mls-ciphertext".as_bytes())
    );
}

fn sample_message() -> MessagePayload {
    MessagePayload {
        message_id: MessageId(7),
        channel_id: ChannelId(3),
        sender_id: shared::domain::UserId(5),
        sender_username: Some("alice".to_string()),
        ciphertext_b64: STANDARD.encode(b"cipher"),
        attachment: None,
        sent_at: "2024-01-01T00:00:00Z".parse().expect("timestamp"),
    }
}

#[test]
fn classify_missing_mls_group_errors_from_all_backends() {
    assert!(is_missing_mls_group_error(
        "no active MLS group available for channel 9"
    ));
    assert!(is_missing_mls_group_error(
        "MLS group not opened for channel 9"
    ));
    assert!(is_missing_mls_group_error(
        "MLS session missing for guild 1 channel 9"
    ));
    assert!(is_missing_mls_group_error("MLS group not initialized"));
    assert!(!is_missing_mls_group_error("failed to connect to server"));
}

#[tokio::test]
async fn emit_decrypted_message_backfills_channel_mapping_from_selected_context() {
    let client = RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::ok(Vec::new(), b"hello".to_vec())),
    );
    {
        let mut inner = client.inner.lock().await;
        inner.user_id = Some(99);
        inner.selected_guild = Some(GuildId(11));
        inner.selected_channel = Some(ChannelId(3));
        inner
            .initialized_mls_channels
            .insert((GuildId(11), ChannelId(3)));
    }

    client
        .emit_decrypted_message(&sample_message())
        .await
        .expect("decrypt should succeed with selected channel fallback");

    let inner = client.inner.lock().await;
    assert_eq!(inner.channel_guilds.get(&ChannelId(3)), Some(&GuildId(11)));
}

#[tokio::test]
async fn emits_decrypted_message_event_for_application_data() {
    let client = RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::ok(Vec::new(), b"hello".to_vec())),
    );
    {
        let mut inner = client.inner.lock().await;
        inner.user_id = Some(99);
        inner.channel_guilds.insert(ChannelId(3), GuildId(11));
        inner
            .initialized_mls_channels
            .insert((GuildId(11), ChannelId(3)));
    }
    let mut rx = client.subscribe_events();

    client
        .emit_decrypted_message(&sample_message())
        .await
        .expect("decrypt should succeed");

    let event = rx.recv().await.expect("event");
    match event {
        ClientEvent::MessageDecrypted { plaintext, .. } => assert_eq!(plaintext, "hello"),
        other => panic!("unexpected event: {other:?}"),
    }
}

#[tokio::test]
async fn emits_plaintext_for_self_echo_without_mls_decrypt() {
    let manager = TestMlsSessionManager::ok(Vec::new(), b"should-not-decrypt".to_vec());
    let decrypt_inputs = manager.decrypted_ciphertexts.clone();
    let client = RealtimeClient::new_with_mls_session_manager(PassthroughCrypto, Arc::new(manager));
    {
        let mut inner = client.inner.lock().await;
        inner.user_id = Some(5);
        inner.channel_guilds.insert(ChannelId(3), GuildId(11));
        inner
            .pending_outbound_plaintexts
            .insert(STANDARD.encode(b"cipher"), "self echo".to_string());
    }
    let mut rx = client.subscribe_events();

    let mut message = sample_message();
    message.sender_id = shared::domain::UserId(5);

    client
        .emit_decrypted_message(&message)
        .await
        .expect("self echo should bypass decrypt");

    let event = rx.recv().await.expect("event");
    match event {
        ClientEvent::MessageDecrypted { plaintext, .. } => assert_eq!(plaintext, "self echo"),
        other => panic!("unexpected event: {other:?}"),
    }

    let decrypt_inputs = decrypt_inputs.lock().await;
    assert!(decrypt_inputs.is_empty());
}

#[tokio::test]
async fn suppresses_non_application_messages_after_decrypt() {
    let client = RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::ok(Vec::new(), Vec::new())),
    );
    {
        let mut inner = client.inner.lock().await;
        inner.user_id = Some(99);
        inner.channel_guilds.insert(ChannelId(3), GuildId(11));
        inner
            .initialized_mls_channels
            .insert((GuildId(11), ChannelId(3)));
    }
    let mut rx = client.subscribe_events();

    client
        .emit_decrypted_message(&sample_message())
        .await
        .expect("decrypt should still succeed");

    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn emit_decrypted_message_skips_when_uninitialized_and_no_welcome() {
    let manager = TestMlsSessionManager::ok(Vec::new(), b"hello".to_vec());
    let decrypt_inputs = manager.decrypted_ciphertexts.clone();
    let open_or_create_calls = manager.open_or_create_calls.clone();
    let client = RealtimeClient::new_with_mls_session_manager(PassthroughCrypto, Arc::new(manager));
    {
        let mut inner = client.inner.lock().await;
        inner.user_id = Some(99);
        inner.channel_guilds.insert(ChannelId(3), GuildId(11));
    }

    client
        .emit_decrypted_message(&sample_message())
        .await
        .expect("should skip ciphertext until welcome exists");

    assert!(decrypt_inputs.lock().await.is_empty());
    assert_eq!(*open_or_create_calls.lock().await, 0);
}

#[tokio::test]
async fn emit_decrypted_message_ignores_unmapped_channel_messages() {
    let manager = TestMlsSessionManager::ok(Vec::new(), b"hello".to_vec());
    let decrypt_inputs = manager.decrypted_ciphertexts.clone();
    let open_or_create_calls = manager.open_or_create_calls.clone();
    let client = RealtimeClient::new_with_mls_session_manager(PassthroughCrypto, Arc::new(manager));
    {
        let mut inner = client.inner.lock().await;
        inner.user_id = Some(99);
        inner.selected_channel = Some(ChannelId(9));
        inner.selected_guild = Some(GuildId(11));
    }

    client
        .emit_decrypted_message(&sample_message())
        .await
        .expect("unmapped channels should be ignored");

    assert!(decrypt_inputs.lock().await.is_empty());
    assert_eq!(*open_or_create_calls.lock().await, 0);
}

#[tokio::test]
async fn emit_decrypted_message_auto_joins_from_welcome_when_uninitialized() {
    let (server_url, server_state) = spawn_onboarding_server().await.expect("spawn server");

    let adder_mls = TestMlsSessionManager::ok(Vec::new(), Vec::new());
    let adder =
        RealtimeClient::new_with_mls_session_manager(PassthroughCrypto, Arc::new(adder_mls));
    {
        let mut inner = adder.inner.lock().await;
        inner.server_url = Some(server_url.clone());
        inner.user_id = Some(7);
        inner.selected_guild = Some(GuildId(11));
        inner.selected_channel = Some(ChannelId(13));
        inner.channel_guilds.insert(ChannelId(13), GuildId(11));
    }

    adder
        .select_channel(ChannelId(13))
        .await
        .expect("adder select");

    let target_mls = TestMlsSessionManager::ok(Vec::new(), b"hello from A".to_vec());
    let joined_welcomes = target_mls.joined_welcomes.clone();
    let decrypt_inputs = target_mls.decrypted_ciphertexts.clone();
    let target =
        RealtimeClient::new_with_mls_session_manager(PassthroughCrypto, Arc::new(target_mls));
    {
        let mut inner = target.inner.lock().await;
        inner.server_url = Some(server_url);
        inner.user_id = Some(42);
        inner.channel_guilds.insert(ChannelId(13), GuildId(11));
    }

    let message = MessagePayload {
        message_id: MessageId(1),
        channel_id: ChannelId(13),
        sender_id: shared::domain::UserId(7),
        sender_username: Some("adder".to_string()),
        ciphertext_b64: STANDARD.encode(b"ciphertext-from-a"),
        attachment: None,
        sent_at: "2024-01-01T00:00:00Z".parse().expect("timestamp"),
    };

    target
        .emit_decrypted_message(&message)
        .await
        .expect("decrypt path should auto-join from welcome");

    assert_eq!(
        joined_welcomes.lock().await.clone(),
        vec![b"welcome-generated".to_vec()]
    );
    assert_eq!(
        decrypt_inputs.lock().await.clone(),
        vec![b"ciphertext-from-a".to_vec()]
    );
    assert_eq!(*server_state.welcome_fetches.lock().await, 1);
}

type BootstrapRequestRecord = (i64, i64, i64, Option<i64>, String);

#[derive(Clone)]
struct OnboardingServerState {
    pending_welcome_b64: Arc<Mutex<Option<String>>>,
    welcome_fetches: Arc<Mutex<u32>>,
    welcome_ready_after_fetches: Arc<Mutex<u32>>,
    add_member_posts: Arc<Mutex<Vec<(i64, i64, i64)>>>,
    stored_ciphertexts: Arc<Mutex<Vec<String>>>,
    include_target_member: Arc<Mutex<bool>>,
    fail_member_fetch: Arc<Mutex<bool>>,
    fail_key_package_fetch: Arc<Mutex<bool>>,
    bootstrap_requests: Arc<Mutex<Vec<BootstrapRequestRecord>>>,
}

async fn onboarding_list_members(
    State(state): State<OnboardingServerState>,
) -> Result<Json<Vec<MemberSummary>>, StatusCode> {
    if *state.fail_member_fetch.lock().await {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let mut members = vec![MemberSummary {
        guild_id: GuildId(11),
        user_id: shared::domain::UserId(7),
        username: "adder".to_string(),
        role: shared::domain::Role::Owner,
        muted: false,
    }];
    if *state.include_target_member.lock().await {
        members.push(MemberSummary {
            guild_id: GuildId(11),
            user_id: shared::domain::UserId(42),
            username: "target".to_string(),
            role: shared::domain::Role::Member,
            muted: false,
        });
    }
    Ok(Json(members))
}

#[derive(Deserialize)]
struct FetchKeyPackageQuery {
    user_id: i64,
}

async fn onboarding_fetch_key_package(
    State(state): State<OnboardingServerState>,
    Query(q): Query<FetchKeyPackageQuery>,
) -> Result<Json<KeyPackageResponse>, StatusCode> {
    if *state.fail_key_package_fetch.lock().await && q.user_id == 42 {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(KeyPackageResponse {
        key_package_id: 1,
        guild_id: 11,
        user_id: 42,
        key_package_b64: STANDARD.encode(b"target-kp"),
    }))
}

#[derive(Deserialize)]
struct StoreWelcomeQuery {
    user_id: i64,
    guild_id: i64,
    channel_id: i64,
    target_user_id: i64,
}

async fn onboarding_store_welcome(
    State(state): State<OnboardingServerState>,
    Query(q): Query<StoreWelcomeQuery>,
    body: axum::body::Bytes,
) -> StatusCode {
    state
        .add_member_posts
        .lock()
        .await
        .push((q.guild_id, q.channel_id, q.target_user_id));
    if q.user_id == 7 {
        let encoded = STANDARD.encode(body);
        *state.pending_welcome_b64.lock().await = Some(encoded);
    }
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct WelcomeQuery {
    user_id: i64,
}

#[derive(Deserialize)]
struct BootstrapRequestQuery {
    user_id: i64,
    guild_id: i64,
    channel_id: i64,
    target_user_id: Option<i64>,
    reason: String,
}

async fn onboarding_bootstrap_request(
    State(state): State<OnboardingServerState>,
    Query(q): Query<BootstrapRequestQuery>,
) -> StatusCode {
    state.bootstrap_requests.lock().await.push((
        q.user_id,
        q.guild_id,
        q.channel_id,
        q.target_user_id,
        q.reason,
    ));
    StatusCode::NO_CONTENT
}

async fn onboarding_fetch_welcome(
    State(state): State<OnboardingServerState>,
    Query(q): Query<WelcomeQuery>,
) -> Result<Json<WelcomeResponse>, StatusCode> {
    if q.user_id != 42 {
        return Err(StatusCode::NOT_FOUND);
    }

    let mut fetches = state.welcome_fetches.lock().await;
    *fetches += 1;
    let ready_after = *state.welcome_ready_after_fetches.lock().await;
    if *fetches <= ready_after {
        return Err(StatusCode::NOT_FOUND);
    }

    let mut guard = state.pending_welcome_b64.lock().await;
    let Some(welcome_b64) = guard.take() else {
        return Err(StatusCode::NOT_FOUND);
    };
    Ok(Json(WelcomeResponse {
        guild_id: GuildId(11),
        channel_id: ChannelId(13),
        user_id: shared::domain::UserId(42),
        welcome_b64,
        consumed_at: None,
    }))
}

async fn onboarding_send_message(
    State(state): State<OnboardingServerState>,
    Json(payload): Json<SendMessageHttpRequest>,
) -> StatusCode {
    state
        .stored_ciphertexts
        .lock()
        .await
        .push(payload.ciphertext_b64);
    StatusCode::NO_CONTENT
}

async fn onboarding_messages_for_channel(
    State(state): State<OnboardingServerState>,
) -> Json<Vec<MessagePayload>> {
    let ciphertext_b64 = state
        .stored_ciphertexts
        .lock()
        .await
        .last()
        .cloned()
        .unwrap_or_default();
    Json(vec![MessagePayload {
        message_id: MessageId(1),
        channel_id: ChannelId(13),
        sender_id: shared::domain::UserId(7),
        sender_username: Some("adder".to_string()),
        ciphertext_b64,
        attachment: None,
        sent_at: "2024-01-01T00:00:00Z".parse().expect("timestamp"),
    }])
}

async fn spawn_onboarding_server() -> Result<(String, OnboardingServerState)> {
    std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let state = OnboardingServerState {
        pending_welcome_b64: Arc::new(Mutex::new(None)),
        welcome_fetches: Arc::new(Mutex::new(0)),
        welcome_ready_after_fetches: Arc::new(Mutex::new(0)),
        add_member_posts: Arc::new(Mutex::new(Vec::new())),
        stored_ciphertexts: Arc::new(Mutex::new(Vec::new())),
        include_target_member: Arc::new(Mutex::new(true)),
        fail_member_fetch: Arc::new(Mutex::new(false)),
        fail_key_package_fetch: Arc::new(Mutex::new(false)),
        bootstrap_requests: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route(
            "/guilds/11/members",
            axum::routing::get(onboarding_list_members),
        )
        .route(
            "/mls/key_packages",
            axum::routing::get(onboarding_fetch_key_package),
        )
        .route(
            "/mls/welcome",
            axum::routing::post(onboarding_store_welcome),
        )
        .route("/mls/welcome", axum::routing::get(onboarding_fetch_welcome))
        .route(
            "/channels/13/messages",
            axum::routing::get(onboarding_messages_for_channel),
        )
        .route(
            "/mls/bootstrap/request",
            axum::routing::post(onboarding_bootstrap_request),
        )
        .route("/messages", axum::routing::post(onboarding_send_message))
        .with_state(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{addr}"), state))
}

#[tokio::test]
async fn added_member_retrieves_pending_welcome_and_auto_joins() {
    let (server_url, server_state) = spawn_onboarding_server().await.expect("spawn server");

    let adder_mls = TestMlsSessionManager::ok(Vec::new(), Vec::new());
    let adder =
        RealtimeClient::new_with_mls_session_manager(PassthroughCrypto, Arc::new(adder_mls));
    {
        let mut inner = adder.inner.lock().await;
        inner.server_url = Some(server_url.clone());
        inner.user_id = Some(7);
        inner.selected_guild = Some(GuildId(11));
        inner.selected_channel = Some(ChannelId(13));
        inner.channel_guilds.insert(ChannelId(13), GuildId(11));
    }

    adder
        .select_channel(ChannelId(13))
        .await
        .expect("adder select");

    let posts = server_state.add_member_posts.lock().await.clone();
    assert_eq!(posts, vec![(11, 13, 42)]);

    let target_mls = TestMlsSessionManager::ok(Vec::new(), Vec::new());
    let joined_welcomes = target_mls.joined_welcomes.clone();
    let target =
        RealtimeClient::new_with_mls_session_manager(PassthroughCrypto, Arc::new(target_mls));
    {
        let mut inner = target.inner.lock().await;
        inner.server_url = Some(server_url);
        inner.user_id = Some(42);
        inner.selected_guild = Some(GuildId(11));
        inner.selected_channel = Some(ChannelId(13));
        inner.channel_guilds.insert(ChannelId(13), GuildId(11));
    }

    target
        .select_channel(ChannelId(13))
        .await
        .expect("target select auto joins");

    let welcomes = joined_welcomes.lock().await.clone();
    assert_eq!(welcomes, vec![b"welcome-generated".to_vec()]);
}

#[tokio::test]
async fn added_member_retries_welcome_sync_until_payload_is_ready() {
    let (server_url, server_state) = spawn_onboarding_server().await.expect("spawn server");

    {
        let mut ready_after = server_state.welcome_ready_after_fetches.lock().await;
        *ready_after = 2;
    }

    let adder_mls = TestMlsSessionManager::ok(Vec::new(), Vec::new());
    let adder =
        RealtimeClient::new_with_mls_session_manager(PassthroughCrypto, Arc::new(adder_mls));
    {
        let mut inner = adder.inner.lock().await;
        inner.server_url = Some(server_url.clone());
        inner.user_id = Some(7);
        inner.selected_guild = Some(GuildId(11));
        inner.selected_channel = Some(ChannelId(13));
        inner.channel_guilds.insert(ChannelId(13), GuildId(11));
    }

    adder
        .select_channel(ChannelId(13))
        .await
        .expect("adder select");

    let target_mls = TestMlsSessionManager::ok(Vec::new(), Vec::new());
    let joined_welcomes = target_mls.joined_welcomes.clone();
    let target =
        RealtimeClient::new_with_mls_session_manager(PassthroughCrypto, Arc::new(target_mls));
    {
        let mut inner = target.inner.lock().await;
        inner.server_url = Some(server_url);
        inner.user_id = Some(42);
        inner.selected_guild = Some(GuildId(11));
        inner.selected_channel = Some(ChannelId(13));
        inner.channel_guilds.insert(ChannelId(13), GuildId(11));
    }

    target
        .select_channel(ChannelId(13))
        .await
        .expect("target select retries until welcome exists");

    let welcomes = joined_welcomes.lock().await.clone();
    assert_eq!(welcomes, vec![b"welcome-generated".to_vec()]);

    let fetches = *server_state.welcome_fetches.lock().await;
    assert!(fetches >= 3);
}

#[tokio::test]
async fn derive_livekit_e2ee_key_is_deterministic_for_same_connection() {
    let client = RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::with_exported_secret(
            b"same-mls-secret-material".to_vec(),
        )),
    );

    let first = client
        .derive_livekit_e2ee_key(GuildId(1), ChannelId(10))
        .await
        .expect("first key");
    let second = client
        .derive_livekit_e2ee_key(GuildId(1), ChannelId(10))
        .await
        .expect("second key");

    assert_eq!(first, second);
}

#[tokio::test]
async fn derive_livekit_e2ee_key_is_domain_separated_by_channel() {
    let client = RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::with_exported_secret(
            b"same-mls-secret-material".to_vec(),
        )),
    );

    let key_a = client
        .derive_livekit_e2ee_key(GuildId(1), ChannelId(10))
        .await
        .expect("key a");
    let key_b = client
        .derive_livekit_e2ee_key(GuildId(1), ChannelId(11))
        .await
        .expect("key b");

    assert_ne!(key_a, key_b);
}

#[tokio::test]
async fn derive_livekit_e2ee_key_surfaces_missing_group_failure() {
    let client = RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::failing("group not initialized")),
    );

    let err = client
        .derive_livekit_e2ee_key(GuildId(9), ChannelId(99))
        .await
        .expect_err("must fail");

    match err {
        LiveKitE2eeKeyError::MissingMlsGroup {
            guild_id,
            channel_id,
        } => {
            assert_eq!(guild_id, 9);
            assert_eq!(channel_id, 99);
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

#[tokio::test]
async fn derive_livekit_e2ee_key_surfaces_export_failure() {
    let client = RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::failing("backend export failed")),
    );

    let err = client
        .derive_livekit_e2ee_key(GuildId(9), ChannelId(100))
        .await
        .expect_err("must fail");

    match err {
        LiveKitE2eeKeyError::ExportFailure {
            guild_id,
            channel_id,
            source,
        } => {
            assert_eq!(guild_id, 9);
            assert_eq!(channel_id, 100);
            assert!(source.to_string().contains("backend export failed"));
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

#[tokio::test]
async fn derive_livekit_e2ee_key_rejects_invalid_cached_key_length() {
    let client = RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::with_exported_secret(
            b"same-mls-secret-material".to_vec(),
        )),
    );

    {
        let mut inner = client.inner.lock().await;
        inner.voice_session_keys.insert(
            VoiceConnectionKey::new(GuildId(5), ChannelId(6)),
            CachedVoiceSessionKey {
                key: vec![7u8; 8],
                expires_at: Instant::now() + Duration::from_secs(30),
            },
        );
    }

    let err = client
        .derive_livekit_e2ee_key(GuildId(5), ChannelId(6))
        .await
        .expect_err("must fail");

    match err {
        LiveKitE2eeKeyError::InvalidDerivedKeyLength { expected, actual } => {
            assert_eq!(expected, LIVEKIT_E2EE_KEY_LEN);
            assert_eq!(actual, 8);
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

#[tokio::test]
async fn onboarding_flow_encrypts_fetches_once_and_renders_plaintext() {
    let (server_url, server_state) = spawn_onboarding_server().await.expect("spawn server");

    let adder = RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::ok(
            b"ciphertext-from-a".to_vec(),
            Vec::new(),
        )),
    );
    {
        let mut inner = adder.inner.lock().await;
        inner.server_url = Some(server_url.clone());
        inner.user_id = Some(7);
        inner.selected_guild = Some(GuildId(11));
        inner.selected_channel = Some(ChannelId(13));
        inner.channel_guilds.insert(ChannelId(13), GuildId(11));
    }

    adder
        .select_channel(ChannelId(13))
        .await
        .expect("adder select");
    adder.send_message("hello from A").await.expect("send text");

    let stored_ciphertexts = server_state.stored_ciphertexts.lock().await.clone();
    assert_eq!(stored_ciphertexts.len(), 2);
    assert_ne!(
        stored_ciphertexts[0],
        STANDARD.encode("hello from A".as_bytes())
    );
    assert_ne!(
        stored_ciphertexts[1],
        STANDARD.encode("hello from A".as_bytes())
    );

    let target_mls = TestMlsSessionManager::ok(Vec::new(), b"hello from A".to_vec());
    let joined_welcomes = target_mls.joined_welcomes.clone();
    let decrypted_ciphertexts = target_mls.decrypted_ciphertexts.clone();
    let target =
        RealtimeClient::new_with_mls_session_manager(PassthroughCrypto, Arc::new(target_mls));
    {
        let mut inner = target.inner.lock().await;
        inner.server_url = Some(server_url.clone());
        inner.user_id = Some(42);
        inner.selected_guild = Some(GuildId(11));
        inner.selected_channel = Some(ChannelId(13));
        inner.channel_guilds.insert(ChannelId(13), GuildId(11));
    }

    let mut rx = target.subscribe_events();
    target
        .select_channel(ChannelId(13))
        .await
        .expect("target select auto joins");

    loop {
        let event = rx.recv().await.expect("decrypted event");
        if let ClientEvent::MessageDecrypted { plaintext, .. } = event {
            assert_eq!(plaintext, "hello from A");
            break;
        }
    }

    let welcomes = joined_welcomes.lock().await.clone();
    assert_eq!(welcomes, vec![b"welcome-generated".to_vec()]);

    let decrypt_inputs = decrypted_ciphertexts.lock().await.clone();
    assert_eq!(decrypt_inputs, vec![b"ciphertext-from-a".to_vec()]);

    let second_welcome_fetch = reqwest::Client::new()
        .get(format!(
            "{server_url}/mls/welcome?user_id=42&guild_id=11&channel_id=13"
        ))
        .send()
        .await
        .expect("second fetch request");
    assert_eq!(second_welcome_fetch.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn moderator_retries_member_bootstrap_after_new_member_joins() {
    let (server_url, server_state) = spawn_onboarding_server().await.expect("spawn server");
    *server_state.include_target_member.lock().await = false;

    let adder = RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::ok(
            b"ciphertext-from-a".to_vec(),
            Vec::new(),
        )),
    );
    {
        let mut inner = adder.inner.lock().await;
        inner.server_url = Some(server_url.clone());
        inner.user_id = Some(7);
        inner.selected_guild = Some(GuildId(11));
        inner.selected_channel = Some(ChannelId(13));
        inner.channel_guilds.insert(ChannelId(13), GuildId(11));
    }

    adder
        .select_channel(ChannelId(13))
        .await
        .expect("adder select");
    assert!(server_state.add_member_posts.lock().await.is_empty());

    *server_state.include_target_member.lock().await = true;
    adder
        .send_message("hello after target joined")
        .await
        .expect("send triggers add-member retry");

    let posts = server_state.add_member_posts.lock().await.clone();
    assert_eq!(posts, vec![(11, 13, 42)]);

    let target_mls = TestMlsSessionManager::ok(Vec::new(), b"hello after target joined".to_vec());
    let joined_welcomes = target_mls.joined_welcomes.clone();
    let target =
        RealtimeClient::new_with_mls_session_manager(PassthroughCrypto, Arc::new(target_mls));
    {
        let mut inner = target.inner.lock().await;
        inner.server_url = Some(server_url);
        inner.user_id = Some(42);
        inner.selected_guild = Some(GuildId(11));
        inner.selected_channel = Some(ChannelId(13));
        inner.channel_guilds.insert(ChannelId(13), GuildId(11));
    }

    target
        .select_channel(ChannelId(13))
        .await
        .expect("target receives deferred welcome");
    assert_eq!(
        joined_welcomes.lock().await.clone(),
        vec![b"welcome-generated".to_vec()]
    );
}

#[tokio::test]
async fn missing_welcome_bootstrap_targets_requester_and_forces_retry_for_that_member() {
    let (server_url, server_state) = spawn_onboarding_server().await.expect("spawn server");

    let requester = RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::ok(Vec::new(), Vec::new())),
    );
    {
        let mut inner = requester.inner.lock().await;
        inner.server_url = Some(server_url.clone());
        inner.user_id = Some(42);
        inner.selected_guild = Some(GuildId(11));
        inner.selected_channel = Some(ChannelId(13));
        inner.channel_guilds.insert(ChannelId(13), GuildId(11));
    }

    requester
        .request_mls_bootstrap(
            GuildId(11),
            ChannelId(13),
            None,
            MlsBootstrapReason::MissingPendingWelcome,
        )
        .await
        .expect("bootstrap request succeeds");

    let bootstrap_requests = server_state.bootstrap_requests.lock().await.clone();
    assert_eq!(bootstrap_requests.len(), 1);
    assert_eq!(
        bootstrap_requests[0],
        (42, 11, 13, Some(42), "missing_pending_welcome".to_string())
    );

    let leader = RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::ok(Vec::new(), Vec::new())),
    );
    {
        let mut inner = leader.inner.lock().await;
        inner.server_url = Some(server_url);
        inner.user_id = Some(7);
        inner.selected_guild = Some(GuildId(11));
        inner.selected_channel = Some(ChannelId(13));
        inner.channel_guilds.insert(ChannelId(13), GuildId(11));
        inner
            .attempted_channel_member_additions
            .insert((GuildId(11), ChannelId(13), 42));
    }

    let bootstrapped = leader
        .maybe_bootstrap_existing_members_if_leader(GuildId(11), ChannelId(13), 7, Some(42))
        .await
        .expect("leader bootstrap call succeeds");
    assert!(bootstrapped);

    // Even though target member was already marked in attempted_channel_member_additions,
    // a targeted retry should still regenerate/store welcome for user 42.
    let posts = server_state.add_member_posts.lock().await.clone();
    assert_eq!(posts, vec![(11, 13, 42)]);
}

#[tokio::test]
async fn bootstrap_membership_fetch_failure_emits_structured_error_and_requests_retry() {
    let (server_url, server_state) = spawn_onboarding_server().await.expect("spawn server");
    *server_state.fail_member_fetch.lock().await = true;

    let client = RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::ok(
            b"ciphertext-no-members".to_vec(),
            Vec::new(),
        )),
    );
    {
        let mut inner = client.inner.lock().await;
        inner.server_url = Some(server_url);
        inner.user_id = Some(7);
        inner.selected_guild = Some(GuildId(11));
        inner.selected_channel = Some(ChannelId(13));
        inner.channel_guilds.insert(ChannelId(13), GuildId(11));
        inner
            .initialized_mls_channels
            .insert((GuildId(11), ChannelId(13)));
    }

    let mut rx = client.subscribe_events();
    client
        .send_message("trigger membership retry")
        .await
        .expect("send should continue despite membership fetch failure");

    let err_msg = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let ClientEvent::Error(msg) = rx.recv().await.expect("event") {
                if msg.contains("category=membership_fetch") {
                    break msg;
                }
            }
        }
    })
    .await
    .expect("structured error event timeout");

    assert!(err_msg.contains("guild_id=11"));
    assert!(err_msg.contains("channel_id=13"));
    assert!(err_msg.contains("actor_user_id=7"));

    let bootstrap_requests = server_state.bootstrap_requests.lock().await.clone();
    assert!(
        bootstrap_requests
            .iter()
            .any(|entry| entry.0 == 7 && entry.1 == 11 && entry.2 == 13),
        "expected a membership-triggered bootstrap retry request"
    );
}

#[tokio::test]
async fn bootstrap_key_package_failure_emits_structured_error_and_requests_targeted_retry() {
    let (server_url, server_state) = spawn_onboarding_server().await.expect("spawn server");
    *server_state.fail_key_package_fetch.lock().await = true;

    let leader = RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::ok(Vec::new(), Vec::new())),
    );
    {
        let mut inner = leader.inner.lock().await;
        inner.server_url = Some(server_url);
        inner.user_id = Some(7);
        inner.selected_guild = Some(GuildId(11));
        inner.selected_channel = Some(ChannelId(13));
        inner.channel_guilds.insert(ChannelId(13), GuildId(11));
    }

    let mut rx = leader.subscribe_events();
    leader
        .select_channel(ChannelId(13))
        .await
        .expect("select channel should succeed even when key package fetch fails");

    let err_msg = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let ClientEvent::Error(msg) = rx.recv().await.expect("event") {
                if msg.contains("category=key_package_fetch") {
                    break msg;
                }
            }
        }
    })
    .await
    .expect("structured error event timeout");

    assert!(err_msg.contains("guild_id=11"));
    assert!(err_msg.contains("channel_id=13"));
    assert!(err_msg.contains("target_user_id=42"));

    let bootstrap_requests = server_state.bootstrap_requests.lock().await.clone();
    assert!(
        bootstrap_requests.iter().any(|entry| {
            entry.0 == 7
                && entry.1 == 11
                && entry.2 == 13
                && entry.3 == Some(42)
                && entry.4 == "unknown"
        }),
        "expected a targeted bootstrap retry request for the key package failure"
    );
}

#[tokio::test]
async fn send_message_refreshes_locally_when_websocket_is_unavailable() {
    let (server_url, _server_state) = spawn_onboarding_server().await.expect("spawn server");

    let client = RealtimeClient::new_with_mls_session_manager(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::ok(
            b"ciphertext-no-ws".to_vec(),
            b"hello without ws".to_vec(),
        )),
    );
    {
        let mut inner = client.inner.lock().await;
        inner.server_url = Some(server_url);
        inner.user_id = Some(7);
        inner.selected_guild = Some(GuildId(11));
        inner.selected_channel = Some(ChannelId(13));
        inner.channel_guilds.insert(ChannelId(13), GuildId(11));
        inner
            .initialized_mls_channels
            .insert((GuildId(11), ChannelId(13)));
        inner.ws_started = false;
    }

    let mut rx = client.subscribe_events();
    client
        .send_message("hello without ws")
        .await
        .expect("send should succeed");

    let event = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let event = rx.recv().await.expect("event");
            if let ClientEvent::MessageDecrypted { plaintext, .. } = event {
                break plaintext;
            }
        }
    })
    .await
    .expect("message event timeout");

    assert_eq!(event, "hello without ws");
}

struct MockLiveKitControlPlane {
    event: ServerEvent,
}

#[async_trait]
impl LiveKitControlPlane for MockLiveKitControlPlane {
    async fn request_livekit_token(&self, request: ClientRequest) -> Result<ServerEvent> {
        match request {
            ClientRequest::RequestLiveKitToken { .. } => Ok(self.event.clone()),
            _ => Err(anyhow!("unexpected request")),
        }
    }
}

struct MockRoom {
    supports_screen: bool,
    e2ee_enabled: bool,
    events_tx: broadcast::Sender<LiveKitRoomEvent>,
    published: Arc<Mutex<Vec<LocalTrack>>>,
    unpublish_calls: Arc<Mutex<u32>>,
    leave_calls: Arc<Mutex<u32>>,
}

#[async_trait]
impl LiveKitRoomSession for MockRoom {
    async fn publish_local_track(&self, track: LocalTrack) -> Result<()> {
        self.published.lock().await.push(track);
        Ok(())
    }

    async fn unpublish_local_tracks(&self) -> Result<()> {
        *self.unpublish_calls.lock().await += 1;
        Ok(())
    }

    async fn leave(&self) -> Result<()> {
        *self.leave_calls.lock().await += 1;
        Ok(())
    }

    fn supports_screen_share(&self) -> bool {
        self.supports_screen
    }

    fn is_e2ee_enabled(&self) -> bool {
        self.e2ee_enabled
    }

    fn subscribe_events(&self) -> broadcast::Receiver<LiveKitRoomEvent> {
        self.events_tx.subscribe()
    }
}

struct MockLiveKitConnector {
    room: Arc<MockRoom>,
    options_seen: Arc<Mutex<Vec<LiveKitRoomOptions>>>,
}

#[async_trait]
impl LiveKitConnectorProvider for MockLiveKitConnector {
    async fn connect_room(
        &self,
        options: LiveKitRoomOptions,
    ) -> Result<Arc<dyn LiveKitRoomSession>> {
        self.options_seen.lock().await.push(options);
        Ok(self.room.clone())
    }
}

#[tokio::test]
async fn connect_and_disconnect_voice_session_flow() {
    let room = Arc::new(MockRoom {
        supports_screen: true,
        e2ee_enabled: true,
        events_tx: broadcast::channel(32).0,
        published: Arc::new(Mutex::new(Vec::new())),
        unpublish_calls: Arc::new(Mutex::new(0)),
        leave_calls: Arc::new(Mutex::new(0)),
    });
    let options_seen = Arc::new(Mutex::new(Vec::new()));
    let client = RealtimeClient::new_with_dependencies(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::with_exported_secret(
            b"voice-secret-material".to_vec(),
        )),
        Arc::new(MockLiveKitControlPlane {
            event: ServerEvent::LiveKitTokenIssued {
                guild_id: GuildId(11),
                channel_id: ChannelId(13),
                room_name: "g:11:c:13".to_string(),
                token: "token-abc".to_string(),
            },
        }),
        Arc::new(MockLiveKitConnector {
            room: room.clone(),
            options_seen: options_seen.clone(),
        }),
    );

    client
        .connect_voice_session(VoiceConnectOptions {
            guild_id: GuildId(11),
            channel_id: ChannelId(13),
            can_publish_mic: true,
            can_publish_screen: true,
        })
        .await
        .expect("connect");

    assert_eq!(room.published.lock().await.len(), 2);
    let seen = options_seen.lock().await;
    assert_eq!(seen.len(), 1);
    assert!(seen[0].e2ee_enabled);
    assert_eq!(seen[0].room_name, "g:11:c:13");

    client.disconnect_voice_session().await.expect("disconnect");
    assert_eq!(*room.unpublish_calls.lock().await, 1);
    assert_eq!(*room.leave_calls.lock().await, 1);
}

#[tokio::test]
async fn voice_participant_events_are_emitted() {
    let room = Arc::new(MockRoom {
        supports_screen: false,
        e2ee_enabled: true,
        events_tx: broadcast::channel(32).0,
        published: Arc::new(Mutex::new(Vec::new())),
        unpublish_calls: Arc::new(Mutex::new(0)),
        leave_calls: Arc::new(Mutex::new(0)),
    });

    let client = RealtimeClient::new_with_dependencies(
        PassthroughCrypto,
        Arc::new(TestMlsSessionManager::with_exported_secret(
            b"voice-secret-material".to_vec(),
        )),
        Arc::new(MockLiveKitControlPlane {
            event: ServerEvent::LiveKitTokenIssued {
                guild_id: GuildId(1),
                channel_id: ChannelId(2),
                room_name: "g:1:c:2".to_string(),
                token: "token".to_string(),
            },
        }),
        Arc::new(MockLiveKitConnector {
            room: room.clone(),
            options_seen: Arc::new(Mutex::new(Vec::new())),
        }),
    );

    let mut rx = client.subscribe_events();
    client
        .connect_voice_session(VoiceConnectOptions {
            guild_id: GuildId(1),
            channel_id: ChannelId(2),
            can_publish_mic: false,
            can_publish_screen: false,
        })
        .await
        .expect("connect");

    let _ = room.events_tx.send(LiveKitRoomEvent::ParticipantJoined(
        livekit_integration::RemoteParticipant {
            participant_id: "p1".to_string(),
            identity: "alice".to_string(),
        },
    ));
    let _ = room.events_tx.send(LiveKitRoomEvent::ParticipantLeft {
        participant_id: "p1".to_string(),
    });

    let mut got_join = false;
    let mut got_leave = false;
    for _ in 0..6 {
        if let Ok(ClientEvent::VoiceParticipantsUpdated { participants, .. }) = rx.recv().await {
            if participants.iter().any(|p| p.participant_id == "p1") {
                got_join = true;
            }
            if participants.is_empty() {
                got_leave = true;
            }
            if got_join && got_leave {
                break;
            }
        }
    }

    assert!(got_join && got_leave);
}
