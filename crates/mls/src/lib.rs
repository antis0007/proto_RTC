use anyhow::{anyhow, Result};
use async_trait::async_trait;
use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::{MemoryStorage, RustCrypto};
use openmls_traits::OpenMlsProvider;
use serde::{Deserialize, Serialize};
use shared::domain::{ChannelId, GuildId};

use tls_codec::{Deserialize as TlsDeserializeTrait, Serialize as TlsSerializeTrait};

const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;
const MLS_SNAPSHOT_SCHEMA_VERSION: i32 = 1;

#[derive(Debug, Clone)]
pub struct PersistedGroupSnapshot {
    pub schema_version: i32,
    pub group_state_blob: Vec<u8>,
    pub key_material_blob: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MlsSnapshotV1 {
    group_id: Vec<u8>,
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
        };
        handle.load_group_if_exists().await?;
        Ok(handle)
    }

    pub async fn load_or_create_group(&mut self) -> Result<()> {
        if self.group.is_none() {
            self.create_group(self.channel_id).await?;
        }
        Ok(())
    }

    pub fn key_package_bytes(&self) -> Result<Vec<u8>> {
        self.identity.key_package_bytes(&self.provider)
    }

    pub async fn create_group(&mut self, channel_id: ChannelId) -> Result<()> {
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
            MlsMessageBodyIn::Welcome(welcome) => welcome,
            _ => return Err(anyhow!("welcome bytes did not contain a Welcome message")),
        };
        let config = MlsGroupJoinConfig::builder().build();
        let staged = StagedWelcome::new_from_welcome(&self.provider, &config, welcome, None)?;
        self.group = Some(staged.into_group(&self.provider)?);
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
        let message_in = MlsMessageIn::tls_deserialize(&mut ciphertext_bytes)?;
        let protocol_message: ProtocolMessage = message_in
            .try_into_protocol_message()
            .map_err(|_| anyhow!("ciphertext did not contain a protocol message"))?;
        let provider = &self.provider;
        let group = self
            .group
            .as_mut()
            .ok_or_else(|| anyhow!("MLS group not initialized"))?;
        let processed = group.process_message(provider, protocol_message)?;

        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app_msg) => Ok(app_msg.into_bytes()),
            ProcessedMessageContent::StagedCommitMessage(staged_commit) => {
                group.merge_staged_commit(provider, *staged_commit)?;
                self.persist_group().await?;
                Ok(Vec::new())
            }
            // Proposal and external/public messages can legitimately appear on the same MLS wire.
            // They do not produce chat plaintext and should be treated as a no-op in the app path.
            _ => Ok(Vec::new()),
        }
    }

    pub fn export_secret(&self, label: &str, len: usize) -> Result<Vec<u8>> {
        let group = self
            .group
            .as_ref()
            .ok_or_else(|| anyhow!("MLS group not initialized"))?;
        Ok(group.export_secret(&self.provider, label, &[], len)?)
    }

    async fn persist_group(&self) -> Result<()> {
        if let Some(group) = &self.group {
            let group_state_blob = serde_json::to_vec(&MlsSnapshotV1 {
                group_id: group.group_id().as_slice().to_vec(),
            })?;
            let key_material_blob = {
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
                serde_json::to_vec(&SerializedProviderState { values })?
            };

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
        }
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

        if snapshot.schema_version != MLS_SNAPSHOT_SCHEMA_VERSION {
            return Err(anyhow!(
                "unsupported MLS snapshot schema version: {}",
                snapshot.schema_version
            ));
        }

        let parsed_state: MlsSnapshotV1 = serde_json::from_slice(&snapshot.group_state_blob)?;
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

        let group_id = GroupId::from_slice(&parsed_state.group_id);
        self.group = MlsGroup::load(self.provider.storage(), &group_id)?;

        if self.group.is_none() {
            return Err(anyhow!(
                "persisted MLS snapshot did not contain a loadable group"
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, sync::Arc};
    use tokio::sync::Mutex;

    type IdentityKey = (i64, String);
    type GroupKey = (i64, String, i64, i64);
    type BlobMap<K, V> = Arc<Mutex<HashMap<K, V>>>;

    #[derive(Default, Clone)]
    struct MemoryStore {
        identities: BlobMap<IdentityKey, Vec<u8>>,
        groups: BlobMap<GroupKey, PersistedGroupSnapshot>,
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
    async fn persisted_identity_survives_restart_and_can_send_after_commit_merge() {
        let guild_id = GuildId(1);
        let channel_id = ChannelId(77);

        let alice_store = MemoryStore::default();
        let bob_store = MemoryStore::default();
        let charlie_store = MemoryStore::default();

        let alice_identity = MlsIdentity::new_with_name(b"alice".to_vec()).expect("alice identity");
        let bob_identity = MlsIdentity::new_with_name(b"bob".to_vec()).expect("bob identity");

        let bob_identity_bytes = bob_identity.to_bytes().expect("serialize bob identity");
        bob_store
            .save_identity_keys(2, "device-bob", &bob_identity_bytes)
            .await
            .expect("persist bob identity");

        let mut alice = MlsGroupHandle::new(
            alice_store,
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

        let mut bob = MlsGroupHandle::new(
            bob_store.clone(),
            2,
            "device-bob",
            guild_id,
            channel_id,
            MlsIdentity::from_bytes(&bob_identity_bytes).expect("decode bob identity"),
        )
        .await
        .expect("bob handle session 1");

        let bob_key_package = bob
            .key_package_bytes()
            .expect("bob key package before restart");

        let (_commit_for_adder, bob_welcome) =
            alice.add_member(&bob_key_package).await.expect("add bob");

        bob.join_group_from_welcome(&bob_welcome.expect("welcome for bob"))
            .await
            .expect("bob joins");

        drop(bob);

        let mut bob = MlsGroupHandle::new(
            bob_store.clone(),
            2,
            "device-bob",
            guild_id,
            channel_id,
            MlsIdentity::from_bytes(
                &bob_store
                    .load_identity_keys(2, "device-bob")
                    .await
                    .expect("reload identity")
                    .expect("identity exists"),
            )
            .expect("decode identity after restart"),
        )
        .await
        .expect("bob handle session 2");

        let charlie_identity =
            MlsIdentity::new_with_name(b"charlie".to_vec()).expect("charlie identity");
        let charlie = MlsGroupHandle::new(
            charlie_store,
            3,
            "device-charlie",
            guild_id,
            channel_id,
            charlie_identity,
        )
        .await
        .expect("charlie handle");
        let charlie_key_package = charlie
            .key_package_bytes()
            .expect("charlie key package bytes");

        let (commit_for_existing_members, _welcome_for_charlie) = alice
            .add_member(&charlie_key_package)
            .await
            .expect("alice adds charlie");

        let merge_plaintext = bob
            .decrypt_application(&commit_for_existing_members)
            .await
            .expect("bob processes commit from prior session state");
        assert!(merge_plaintext.is_empty());

        let bob_ciphertext = bob
            .encrypt_application(b"bob survived restart")
            .expect("bob encrypts with restored signer");
        let alice_plaintext = alice
            .decrypt_application(&bob_ciphertext)
            .await
            .expect("alice decrypts bob message after restart");
        assert_eq!(alice_plaintext, b"bob survived restart");
    }

    #[tokio::test]
    async fn merge_commit_before_decrypting_next_application() {
        let guild_id = GuildId(1);
        let channel_id = ChannelId(20);

        let alice_identity = MlsIdentity::new_with_name(b"alice".to_vec()).expect("alice identity");
        let bob_identity = MlsIdentity::new_with_name(b"bob".to_vec()).expect("bob identity");
        let charlie_identity =
            MlsIdentity::new_with_name(b"charlie".to_vec()).expect("charlie identity");

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
        let bob = MlsGroupHandle::new(
            MemoryStore::default(),
            2,
            "device-bob",
            guild_id,
            channel_id,
            bob_identity,
        )
        .await
        .expect("bob handle");
        let charlie = MlsGroupHandle::new(
            MemoryStore::default(),
            3,
            "device-charlie",
            guild_id,
            channel_id,
            charlie_identity,
        )
        .await
        .expect("charlie handle");

        let bob_kp = bob.key_package_bytes().expect("bob key package bytes");
        let charlie_kp = charlie
            .key_package_bytes()
            .expect("charlie key package bytes");
        alice.create_group(channel_id).await.expect("create group");

        let (commit_to_bob, welcome_for_bob) = alice.add_member(&bob_kp).await.expect("add bob");

        let mut bob = bob;
        bob.join_group_from_welcome(&welcome_for_bob.expect("welcome bob"))
            .await
            .expect("bob joins");

        let (commit_for_existing, _welcome_for_charlie) =
            alice.add_member(&charlie_kp).await.expect("add charlie");

        let _ = bob
            .decrypt_application(&commit_for_existing)
            .await
            .expect("process inbound commit and merge it");

        let ciphertext = alice
            .encrypt_application(b"post-commit message")
            .expect("encrypt app message");
        let plaintext = bob
            .decrypt_application(&ciphertext)
            .await
            .expect("decrypt app message after commit merge");

        assert_eq!(plaintext, b"post-commit message");
        assert!(!commit_to_bob.is_empty());
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
        let bob = MlsGroupHandle::new(
            MemoryStore::default(),
            2,
            "device-bob",
            guild_id,
            channel_id,
            bob_identity,
        )
        .await
        .expect("bob handle");
        let bob_kp = bob.key_package_bytes().expect("bob key package bytes");
        alice.create_group(channel_id).await.expect("create group");

        let (_commit, welcome) = alice.add_member(&bob_kp).await.expect("add member");
        let welcome = welcome.expect("welcome message");

        let mut bob = bob;
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
    async fn persisted_group_snapshot_round_trip_supports_future_epochs() {
        let guild_id = GuildId(1);
        let channel_id = ChannelId(42);
        let alice_store = MemoryStore::default();
        let bob_store = MemoryStore::default();

        let alice_identity = MlsIdentity::new_with_name(b"alice".to_vec()).expect("alice identity");
        let bob_identity = MlsIdentity::new_with_name(b"bob".to_vec()).expect("bob identity");

        let mut alice = MlsGroupHandle::new(
            alice_store.clone(),
            1,
            "device-alice",
            guild_id,
            channel_id,
            alice_identity,
        )
        .await
        .expect("alice handle");
        let mut bob = MlsGroupHandle::new(
            bob_store.clone(),
            2,
            "device-bob",
            guild_id,
            channel_id,
            bob_identity,
        )
        .await
        .expect("bob handle");

        let bob_kp = bob.key_package_bytes().expect("bob key package");
        alice.create_group(channel_id).await.expect("create group");
        let (_commit, welcome) = alice.add_member(&bob_kp).await.expect("add bob");
        bob.join_group_from_welcome(&welcome.expect("welcome"))
            .await
            .expect("bob joins");

        let pre_restart = alice
            .encrypt_application(b"before restart")
            .expect("encrypt pre-restart");
        let pre_restart_plain = bob
            .decrypt_application(&pre_restart)
            .await
            .expect("decrypt pre-restart");
        assert_eq!(pre_restart_plain, b"before restart");

        let bob_identity_reloaded =
            MlsIdentity::new_with_name(b"bob".to_vec()).expect("bob identity reload");
        let mut bob = MlsGroupHandle::new(
            bob_store.clone(),
            2,
            "device-bob",
            guild_id,
            channel_id,
            bob_identity_reloaded,
        )
        .await
        .expect("bob handle reload");

        let charlie_identity =
            MlsIdentity::new_with_name(b"charlie".to_vec()).expect("charlie identity");
        let charlie = MlsGroupHandle::new(
            MemoryStore::default(),
            3,
            "device-charlie",
            guild_id,
            channel_id,
            charlie_identity,
        )
        .await
        .expect("charlie handle");
        let charlie_kp = charlie.key_package_bytes().expect("charlie key package");

        let (commit_for_existing, _welcome_for_charlie) =
            alice.add_member(&charlie_kp).await.expect("add charlie");

        let merge_result = bob
            .decrypt_application(&commit_for_existing)
            .await
            .expect("process commit after restart");
        assert!(merge_result.is_empty());

        let future_epoch_ciphertext = alice
            .encrypt_application(b"new epoch message")
            .expect("encrypt new epoch");
        let future_epoch_plaintext = bob
            .decrypt_application(&future_epoch_ciphertext)
            .await
            .expect("decrypt new epoch");
        assert_eq!(future_epoch_plaintext, b"new epoch message");
    }
}
