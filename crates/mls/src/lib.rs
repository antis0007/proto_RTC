use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::{MemoryStorage, RustCrypto};
use openmls_traits::OpenMlsProvider;
use serde::{Deserialize, Serialize};
use shared::domain::{ChannelId, GuildId};
use tls_codec::{Deserialize as TlsDeserializeTrait, Serialize as TlsSerializeTrait};

const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;
const MLS_SNAPSHOT_SCHEMA_VERSION: i32 = 2; // bumped

#[derive(Debug, Clone)]
pub struct PersistedGroupSnapshot {
    pub schema_version: i32,
    pub group_state_blob: Vec<u8>,
    pub key_material_blob: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MlsSnapshotV2 {
    /// None => pending join provider state only (no group yet)
    /// Some(...) => persisted group snapshot
    group_id: Option<Vec<u8>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializedProviderState {
    values: Vec<SerializedProviderValue>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializedProviderValue {
    key: Vec<u8>,
    value: Vec<u8>,
}

#[derive(Debug)]
pub struct MlsIdentity {
    credential_with_key: CredentialWithKey,
    signer: SignatureKeyPair,
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializedIdentityV1 {
    credential_with_key: CredentialWithKey,
    signer: SignatureKeyPair,
}

#[derive(Serialize)]
struct SerializedIdentityV1Ref<'a> {
    credential_with_key: &'a CredentialWithKey,
    signer: &'a SignatureKeyPair,
}

impl MlsIdentity {
    pub fn new() -> Result<Self> {
        Self::new_with_name(b"device".to_vec())
    }

    pub fn new_with_name(name: impl Into<Vec<u8>>) -> Result<Self> {
        let credential = BasicCredential::new(name.into());
        let signer = SignatureKeyPair::new(SignatureScheme::ED25519)?;
        let credential_with_key = CredentialWithKey {
            credential: credential.into(),
            signature_key: signer.to_public_vec().into(),
        };

        Ok(Self {
            credential_with_key,
            signer,
        })
    }

    pub fn key_package<P: OpenMlsProvider>(&self, provider: &P) -> Result<KeyPackage> {
        let bundle = KeyPackage::builder().build(
            CIPHERSUITE,
            provider,
            &self.signer,
            self.credential_with_key.clone(),
        )?;
        Ok(bundle.key_package().clone())
    }

    pub fn key_package_bytes<P: OpenMlsProvider>(&self, provider: &P) -> Result<Vec<u8>> {
        Ok(self.key_package(provider)?.tls_serialize_detached()?)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&SerializedIdentityV1Ref {
            credential_with_key: &self.credential_with_key,
            signer: &self.signer,
        })?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let decoded: SerializedIdentityV1 = serde_json::from_slice(bytes)?;
        Ok(Self {
            credential_with_key: decoded.credential_with_key,
            signer: decoded.signer,
        })
    }
}

#[async_trait]
pub trait MlsStore: Send + Sync {
    async fn save_identity_keys(
        &self,
        user_id: i64,
        device_id: &str,
        identity_bytes: &[u8],
    ) -> Result<()>;

    async fn load_identity_keys(&self, user_id: i64, device_id: &str) -> Result<Option<Vec<u8>>>;

    async fn save_group_state(
        &self,
        user_id: i64,
        device_id: &str,
        guild_id: GuildId,
        channel_id: ChannelId,
        snapshot: PersistedGroupSnapshot,
    ) -> Result<()>;

    async fn load_group_state(
        &self,
        user_id: i64,
        device_id: &str,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Result<Option<PersistedGroupSnapshot>>;

    /// Pending join state stores provider key material for a generated KeyPackage
    /// before a Welcome is received/applied.
    async fn save_pending_join_state(
        &self,
        user_id: i64,
        device_id: &str,
        guild_id: GuildId,
        snapshot: PersistedGroupSnapshot,
    ) -> Result<()>;

    async fn load_pending_join_state(
        &self,
        user_id: i64,
        device_id: &str,
        guild_id: GuildId,
    ) -> Result<Option<PersistedGroupSnapshot>>;

    async fn clear_pending_join_state(
        &self,
        user_id: i64,
        device_id: &str,
        guild_id: GuildId,
    ) -> Result<()>;
}

#[derive(Default, Debug)]
pub struct PersistentOpenMlsProvider {
    crypto: RustCrypto,
    storage: MemoryStorage,
}

impl OpenMlsProvider for PersistentOpenMlsProvider {
    type CryptoProvider = RustCrypto;
    type RandProvider = RustCrypto;
    type StorageProvider = MemoryStorage;

    fn storage(&self) -> &Self::StorageProvider {
        &self.storage
    }

    fn crypto(&self) -> &Self::CryptoProvider {
        &self.crypto
    }

    fn rand(&self) -> &Self::RandProvider {
        &self.crypto
    }
}

pub struct MlsGroupHandle<S: MlsStore> {
    store: S,
    user_id: i64,
    device_id: String,
    guild_id: GuildId,
    channel_id: ChannelId,
    provider: PersistentOpenMlsProvider,
    identity: MlsIdentity,
    group: Option<MlsGroup>,
    /// true when this handle was created specifically to consume a Welcome using
    /// previously persisted pending key material.
    join_only_mode: bool,
}

impl<S: MlsStore> MlsGroupHandle<S> {
    pub async fn new(
        store: S,
        user_id: i64,
        device_id: impl Into<String>,
        guild_id: GuildId,
        channel_id: ChannelId,
        identity: MlsIdentity,
    ) -> Result<Self> {
        let mut handle = Self {
            store,
            user_id,
            device_id: device_id.into(),
            guild_id,
            channel_id,
            provider: PersistentOpenMlsProvider::default(),
            identity,
            group: None,
            join_only_mode: false,
        };
        handle.load_group_if_exists().await?;
        Ok(handle)
    }

    /// Constructor for Welcome consumption.
    /// Loads pending provider key material (HPKE init private key, etc.) that was
    /// saved when the KeyPackage was generated.
    pub async fn open_for_join(
        store: S,
        user_id: i64,
        device_id: impl Into<String>,
        guild_id: GuildId,
        channel_id: ChannelId,
        identity: MlsIdentity,
    ) -> Result<Self> {
        let mut handle = Self {
            store,
            user_id,
            device_id: device_id.into(),
            guild_id,
            channel_id,
            provider: PersistentOpenMlsProvider::default(),
            identity,
            group: None,
            join_only_mode: true,
        };

        // Load pending key package/provider state. This is required to process Welcome.
        handle
            .load_pending_join_provider_state()
            .await
            .with_context(|| {
                format!(
                    "failed to load pending MLS join state for user={} device={} guild={}",
                    handle.user_id, handle.device_id, handle.guild_id.0
                )
            })?;

        Ok(handle)
    }

    pub async fn load_or_create_group(&mut self) -> Result<()> {
        if self.group.is_none() {
            self.create_group(self.channel_id).await?;
        }
        Ok(())
    }

    /// Generate a KeyPackage and persist the provider key material required to later
    /// consume a Welcome in a different process/handle.
    pub async fn key_package_bytes(&mut self) -> Result<Vec<u8>> {
        let key_package = self.identity.key_package_bytes(&self.provider)?;
        self.persist_pending_join_provider_state().await?;
        Ok(key_package)
    }

    pub async fn create_group(&mut self, channel_id: ChannelId) -> Result<()> {
        if self.join_only_mode {
            return Err(anyhow!(
                "cannot create group while handle is in join-only mode for guild={} channel={}",
                self.guild_id.0,
                self.channel_id.0
            ));
        }

        let group_id = GroupId::from_slice(&channel_id.0.to_le_bytes());
        let config = MlsGroupCreateConfig::builder()
            .ciphersuite(CIPHERSUITE)
            .use_ratchet_tree_extension(true)
            .build();

        let group = MlsGroup::new_with_group_id(
            &self.provider,
            &self.identity.signer,
            &config,
            group_id,
            self.identity.credential_with_key.clone(),
        )?;

        self.group = Some(group);
        self.persist_group().await
    }

    pub async fn join_group_from_welcome(&mut self, welcome_bytes: &[u8]) -> Result<()> {
        let mut bytes = welcome_bytes;
        let welcome_message = MlsMessageIn::tls_deserialize(&mut bytes)?;
        if !bytes.is_empty() {
            return Err(anyhow!("welcome bytes had trailing data"));
        }

        let welcome = match welcome_message.extract() {
            MlsMessageBodyIn::Welcome(w) => w,
            _ => return Err(anyhow!("welcome bytes did not contain a Welcome message")),
        };

        let config = MlsGroupJoinConfig::builder()
            .use_ratchet_tree_extension(true)
            .build();

        let staged = StagedWelcome::new_from_welcome(&self.provider, &config, welcome, None)
            .map_err(|e| anyhow!("failed to stage welcome: {e}"))?;

        let group = staged
            .into_group(&self.provider)
            .map_err(|e| anyhow!("failed to finalize welcome into group: {e}"))?;

        self.group = Some(group);

        // Now that join succeeded, clear pending join state and persist actual group snapshot.
        self.store
            .clear_pending_join_state(self.user_id, &self.device_id, self.guild_id)
            .await?;
        self.persist_group().await
    }

    pub async fn add_member(
        &mut self,
        key_package_bytes: &[u8],
    ) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
        let mut bytes = key_package_bytes;
        let key_package_in = <KeyPackageIn as TlsDeserializeTrait>::tls_deserialize(&mut bytes)?;
        if !bytes.is_empty() {
            return Err(anyhow!("key package bytes had trailing data"));
        }

        let provider = &self.provider;
        let key_package = key_package_in
            .validate(provider.crypto(), ProtocolVersion::default())
            .map_err(|e| anyhow!("invalid key package bytes: {e}"))?;

        let signer = &self.identity.signer;
        let group = self
            .group
            .as_mut()
            .ok_or_else(|| anyhow!("MLS group not initialized"))?;

        let (commit, welcome, _group_info) = group.add_members(provider, signer, &[key_package])?;

        let commit_bytes = commit.tls_serialize_detached()?;
        let welcome_bytes = Some(welcome.tls_serialize_detached()?);

        group.merge_pending_commit(provider)?;
        self.persist_group().await?;

        Ok((commit_bytes, welcome_bytes))
    }

    pub fn group_contains_key_package_identity(&self, key_package_bytes: &[u8]) -> Result<bool> {
        let mut bytes = key_package_bytes;
        let key_package_in = <KeyPackageIn as TlsDeserializeTrait>::tls_deserialize(&mut bytes)?;
        if !bytes.is_empty() {
            return Err(anyhow!("key package bytes had trailing data"));
        }

        let key_package = key_package_in
            .validate(self.provider.crypto(), ProtocolVersion::default())
            .map_err(|e| anyhow!("invalid key package bytes: {e}"))?;

        let target_signature_key = key_package.leaf_node().signature_key().as_slice();

        let group = self
            .group
            .as_ref()
            .ok_or_else(|| anyhow!("MLS group not initialized"))?;

        Ok(group
            .members()
            .any(|member| member.signature_key.as_slice() == target_signature_key))
    }

    pub fn encrypt_application(&mut self, plaintext_bytes: &[u8]) -> Result<Vec<u8>> {
        let provider = &self.provider;
        let signer = &self.identity.signer;
        let group = self
            .group
            .as_mut()
            .ok_or_else(|| anyhow!("MLS group not initialized"))?;

        let msg = group.create_message(provider, signer, plaintext_bytes)?;
        Ok(msg.tls_serialize_detached()?)
    }

    pub async fn decrypt_application(&mut self, ciphertext_bytes: &[u8]) -> Result<Vec<u8>> {
        let mut ciphertext_bytes = ciphertext_bytes;
        let message_in = match MlsMessageIn::tls_deserialize(&mut ciphertext_bytes) {
            Ok(m) => m,
            Err(e) => {
                let s = e.to_string();
                if s.contains("SecretReuseError") || s.contains("requested secret was deleted") {
                    return Ok(Vec::new());
                }
                return Err(anyhow!("failed to deserialize MLS message: {e}"));
            }
        };

        let protocol_message: ProtocolMessage = match message_in.try_into_protocol_message() {
            Ok(p) => p,
            Err(_) => return Err(anyhow!("ciphertext did not contain a protocol message")),
        };

        let provider = &self.provider;
        let group = self
            .group
            .as_mut()
            .ok_or_else(|| anyhow!("MLS group not initialized"))?;

        let processed = match group.process_message(provider, protocol_message) {
            Ok(p) => p,
            Err(e) => {
                let s = e.to_string();
                // Replay / FS-pruned secrets can happen in history fetch paths and are safe to skip.
                if s.contains("SecretReuseError") || s.contains("requested secret was deleted") {
                    return Ok(Vec::new());
                }
                return Err(anyhow!("failed to process MLS message: {e}"));
            }
        };

        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app_msg) => Ok(app_msg.into_bytes()),
            ProcessedMessageContent::StagedCommitMessage(staged_commit) => {
                group.merge_staged_commit(provider, *staged_commit)?;
                self.persist_group().await?;
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }

    pub fn export_secret(&self, label: &str, len: usize) -> Result<Vec<u8>> {
        let group = self
            .group
            .as_ref()
            .ok_or_else(|| anyhow!("MLS group not initialized"))?;

        Ok(group.export_secret(self.provider.crypto(), label, &[], len)?)
    }

    async fn persist_group(&self) -> Result<()> {
        let Some(group) = &self.group else {
            return Ok(());
        };

        let group_state_blob = serde_json::to_vec(&MlsSnapshotV2 {
            group_id: Some(group.group_id().as_slice().to_vec()),
        })?;

        let key_material_blob = self.serialize_provider_state()?;

        self.store
            .save_group_state(
                self.user_id,
                &self.device_id,
                self.guild_id,
                self.channel_id,
                PersistedGroupSnapshot {
                    schema_version: MLS_SNAPSHOT_SCHEMA_VERSION,
                    group_state_blob,
                    key_material_blob,
                },
            )
            .await?;

        Ok(())
    }

    async fn persist_pending_join_provider_state(&self) -> Result<()> {
        let group_state_blob = serde_json::to_vec(&MlsSnapshotV2 { group_id: None })?;
        let key_material_blob = self.serialize_provider_state()?;

        self.store
            .save_pending_join_state(
                self.user_id,
                &self.device_id,
                self.guild_id,
                PersistedGroupSnapshot {
                    schema_version: MLS_SNAPSHOT_SCHEMA_VERSION,
                    group_state_blob,
                    key_material_blob,
                },
            )
            .await?;

        Ok(())
    }

    async fn load_group_if_exists(&mut self) -> Result<()> {
        let Some(snapshot) = self
            .store
            .load_group_state(
                self.user_id,
                &self.device_id,
                self.guild_id,
                self.channel_id,
            )
            .await?
        else {
            return Ok(());
        };

        self.restore_from_snapshot(snapshot, true).await
    }

    async fn load_pending_join_provider_state(&mut self) -> Result<()> {
        let Some(snapshot) = self
            .store
            .load_pending_join_state(self.user_id, &self.device_id, self.guild_id)
            .await?
        else {
            return Err(anyhow!(
                "no pending join state found (key package may not have been generated/persisted yet)"
            ));
        };

        self.restore_from_snapshot(snapshot, false).await
    }

    async fn restore_from_snapshot(
        &mut self,
        snapshot: PersistedGroupSnapshot,
        expect_group: bool,
    ) -> Result<()> {
        if snapshot.schema_version != MLS_SNAPSHOT_SCHEMA_VERSION {
            return Err(anyhow!(
                "unsupported MLS snapshot schema version: {}",
                snapshot.schema_version
            ));
        }

        let parsed_state: MlsSnapshotV2 = serde_json::from_slice(&snapshot.group_state_blob)?;
        let parsed_provider_state: SerializedProviderState =
            serde_json::from_slice(&snapshot.key_material_blob)?;

        {
            let mut values = self.provider.storage().values.write().unwrap();
            values.clear();
            values.extend(
                parsed_provider_state
                    .values
                    .into_iter()
                    .map(|entry| (entry.key, entry.value)),
            );
        }

        match (expect_group, parsed_state.group_id) {
            (true, Some(group_id_bytes)) => {
                let group_id = GroupId::from_slice(&group_id_bytes);
                self.group = MlsGroup::load(self.provider.storage(), &group_id)?;

                if self.group.is_none() {
                    return Err(anyhow!(
                        "persisted MLS snapshot did not contain a loadable group"
                    ));
                }
            }
            (true, None) => {
                return Err(anyhow!(
                    "expected group snapshot but found pending-join snapshot"
                ));
            }
            (false, Some(_)) => {
                return Err(anyhow!(
                    "expected pending-join snapshot but found group snapshot"
                ));
            }
            (false, None) => {
                // Correct: provider state restored, group remains None until Welcome is applied.
                self.group = None;
            }
        }

        Ok(())
    }

    fn serialize_provider_state(&self) -> Result<Vec<u8>> {
        let values = self
            .provider
            .storage()
            .values
            .read()
            .unwrap()
            .iter()
            .map(|(key, value)| SerializedProviderValue {
                key: key.clone(),
                value: value.clone(),
            })
            .collect();

        Ok(serde_json::to_vec(&SerializedProviderState { values })?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, sync::Arc};
    use tokio::sync::Mutex;

    type IdentityKey = (i64, String);
    type GroupKey = (i64, String, i64, i64);
    type PendingJoinKey = (i64, String, i64);
    type BlobMap<K, V> = Arc<Mutex<HashMap<K, V>>>;

    #[derive(Default, Clone)]
    struct MemoryStore {
        identities: BlobMap<IdentityKey, Vec<u8>>,
        groups: BlobMap<GroupKey, PersistedGroupSnapshot>,
        pending_joins: BlobMap<PendingJoinKey, PersistedGroupSnapshot>,
    }

    #[async_trait]
    impl MlsStore for MemoryStore {
        async fn save_identity_keys(
            &self,
            user_id: i64,
            device_id: &str,
            identity_bytes: &[u8],
        ) -> Result<()> {
            self.identities
                .lock()
                .await
                .insert((user_id, device_id.to_string()), identity_bytes.to_vec());
            Ok(())
        }

        async fn load_identity_keys(
            &self,
            user_id: i64,
            device_id: &str,
        ) -> Result<Option<Vec<u8>>> {
            Ok(self
                .identities
                .lock()
                .await
                .get(&(user_id, device_id.to_string()))
                .cloned())
        }

        async fn save_group_state(
            &self,
            user_id: i64,
            device_id: &str,
            guild_id: GuildId,
            channel_id: ChannelId,
            snapshot: PersistedGroupSnapshot,
        ) -> Result<()> {
            self.groups.lock().await.insert(
                (user_id, device_id.to_string(), guild_id.0, channel_id.0),
                snapshot,
            );
            Ok(())
        }

        async fn load_group_state(
            &self,
            user_id: i64,
            device_id: &str,
            guild_id: GuildId,
            channel_id: ChannelId,
        ) -> Result<Option<PersistedGroupSnapshot>> {
            Ok(self
                .groups
                .lock()
                .await
                .get(&(user_id, device_id.to_string(), guild_id.0, channel_id.0))
                .cloned())
        }

        async fn save_pending_join_state(
            &self,
            user_id: i64,
            device_id: &str,
            guild_id: GuildId,
            snapshot: PersistedGroupSnapshot,
        ) -> Result<()> {
            self.pending_joins
                .lock()
                .await
                .insert((user_id, device_id.to_string(), guild_id.0), snapshot);
            Ok(())
        }

        async fn load_pending_join_state(
            &self,
            user_id: i64,
            device_id: &str,
            guild_id: GuildId,
        ) -> Result<Option<PersistedGroupSnapshot>> {
            Ok(self
                .pending_joins
                .lock()
                .await
                .get(&(user_id, device_id.to_string(), guild_id.0))
                .cloned())
        }

        async fn clear_pending_join_state(
            &self,
            user_id: i64,
            device_id: &str,
            guild_id: GuildId,
        ) -> Result<()> {
            self.pending_joins
                .lock()
                .await
                .remove(&(user_id, device_id.to_string(), guild_id.0));
            Ok(())
        }
    }

    #[test]
    fn identity_serialization_round_trip_preserves_signing_material() {
        let provider = PersistentOpenMlsProvider::default();
        let identity = MlsIdentity::new_with_name(b"alice".to_vec()).expect("identity");
        let original_key_package = identity
            .key_package_bytes(&provider)
            .expect("original key package");

        let encoded = identity.to_bytes().expect("serialize identity");
        let decoded = MlsIdentity::from_bytes(&encoded).expect("deserialize identity");

        let decoded_key_package = decoded
            .key_package_bytes(&provider)
            .expect("decoded key package");

        assert!(!original_key_package.is_empty());
        assert!(!decoded_key_package.is_empty());
        assert_eq!(identity.to_bytes().expect("serialize original"), encoded);
        assert_eq!(decoded.to_bytes().expect("serialize decoded"), encoded);
    }

    #[tokio::test]
    async fn two_members_exchange_application_message() {
        let guild_id = GuildId(1);
        let channel_id = ChannelId(10);

        let alice_identity = MlsIdentity::new_with_name(b"alice".to_vec()).expect("alice identity");
        let bob_identity = MlsIdentity::new_with_name(b"bob".to_vec()).expect("bob identity");

        let mut alice = MlsGroupHandle::new(
            MemoryStore::default(),
            1,
            "device-alice",
            guild_id,
            channel_id,
            alice_identity,
        )
        .await
        .expect("alice handle");

        let mut bob = MlsGroupHandle::new(
            MemoryStore::default(),
            2,
            "device-bob",
            guild_id,
            channel_id,
            bob_identity,
        )
        .await
        .expect("bob handle");

        let bob_kp = bob
            .key_package_bytes()
            .await
            .expect("bob key package bytes");
        alice.create_group(channel_id).await.expect("create group");

        let (_commit, welcome) = alice.add_member(&bob_kp).await.expect("add member");
        let welcome = welcome.expect("welcome message");

        // Simulate new join handle (different process/instance) using persisted pending join state
        let bob_join = MlsGroupHandle::open_for_join(
            MemoryStore::default(), // NOTE: in this test this is a different store; real test below uses same store
            2,
            "device-bob",
            guild_id,
            channel_id,
            MlsIdentity::new_with_name(b"bob".to_vec()).expect("bob identity"),
        )
        .await;

        // Expected to fail with a different store instance:
        assert!(bob_join.is_err());

        // Join with the original bob handle (same provider state in memory)
        bob.join_group_from_welcome(&welcome)
            .await
            .expect("bob joins group");

        let ciphertext = alice
            .encrypt_application(b"hello bob")
            .expect("encrypt app message");

        let plaintext = bob
            .decrypt_application(&ciphertext)
            .await
            .expect("decrypt app message");

        assert_eq!(plaintext, b"hello bob");
    }

    #[tokio::test]
    async fn decrypt_application_returns_error_when_commit_epoch_is_missing() {
        let guild_id = GuildId(1);
        let channel_id = ChannelId(99);

        let store = MemoryStore::default();

        let mut alice = MlsGroupHandle::new(
            store.clone(),
            1,
            "device-alice",
            guild_id,
            channel_id,
            MlsIdentity::new_with_name(b"alice".to_vec()).expect("alice identity"),
        )
        .await
        .expect("alice handle");
        alice
            .create_group(channel_id)
            .await
            .expect("create alice group");

        let mut bob = MlsGroupHandle::new(
            store.clone(),
            2,
            "device-bob",
            guild_id,
            channel_id,
            MlsIdentity::new_with_name(b"bob".to_vec()).expect("bob identity"),
        )
        .await
        .expect("bob handle");

        let bob_kp = bob.key_package_bytes().await.expect("bob key package");
        let (_commit_ab, welcome_ab) = alice.add_member(&bob_kp).await.expect("add bob");
        bob.join_group_from_welcome(&welcome_ab.expect("welcome bob"))
            .await
            .expect("bob joins");

        let mut charlie = MlsGroupHandle::new(
            store.clone(),
            3,
            "device-charlie",
            guild_id,
            channel_id,
            MlsIdentity::new_with_name(b"charlie".to_vec()).expect("charlie identity"),
        )
        .await
        .expect("charlie handle");
        let charlie_kp = charlie
            .key_package_bytes()
            .await
            .expect("charlie key package");

        let (_commit_ac, _welcome_ac) = alice
            .add_member(&charlie_kp)
            .await
            .expect("add charlie and advance epoch");

        // Bob never processes Alice's commit that advanced the epoch.
        let ct = alice
            .encrypt_application(b"hello after epoch advance")
            .expect("alice encrypt");
        let err = bob
            .decrypt_application(&ct)
            .await
            .expect_err("bob should fail when missing commit epoch");

        assert!(
            err.to_string().contains("Wrong Epoch")
                || err
                    .to_string()
                    .contains("Message epoch differs from the group's epoch"),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test]
    async fn pending_join_state_survives_reopen_and_welcome_join() {
        let guild_id = GuildId(1);
        let channel_id = ChannelId(77);

        let store = MemoryStore::default();

        let alice_identity = MlsIdentity::new_with_name(b"alice".to_vec()).expect("alice identity");
        let bob_identity = MlsIdentity::new_with_name(b"bob".to_vec()).expect("bob identity");

        let mut alice = MlsGroupHandle::new(
            store.clone(),
            1,
            "device-alice",
            guild_id,
            channel_id,
            alice_identity,
        )
        .await
        .expect("alice handle");
        alice
            .create_group(channel_id)
            .await
            .expect("create alice group");

        let mut bob_gen = MlsGroupHandle::new(
            store.clone(),
            2,
            "device-bob",
            guild_id,
            channel_id,
            bob_identity,
        )
        .await
        .expect("bob handle (generate kp)");

        // Generate and persist pending join provider state
        let bob_kp = bob_gen.key_package_bytes().await.expect("bob key package");
        drop(bob_gen);

        let (_commit, welcome) = alice.add_member(&bob_kp).await.expect("alice add bob");
        let welcome = welcome.expect("welcome");

        // Re-open in join mode (simulating separate process)
        let mut bob_join = MlsGroupHandle::open_for_join(
            store.clone(),
            2,
            "device-bob",
            guild_id,
            channel_id,
            MlsIdentity::new_with_name(b"bob".to_vec()).expect("bob identity reload"),
        )
        .await
        .expect("open_for_join");

        bob_join
            .join_group_from_welcome(&welcome)
            .await
            .expect("join from welcome");

        let ct = alice
            .encrypt_application(b"hello after reopen")
            .expect("alice encrypt");
        let pt = bob_join
            .decrypt_application(&ct)
            .await
            .expect("bob decrypt");
        assert_eq!(pt, b"hello after reopen");
    }
}
