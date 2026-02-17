use anyhow::{anyhow, Result};
use async_trait::async_trait;
use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use shared::domain::{ChannelId, GuildId};
use tls_codec::{Deserialize as TlsDeserializeTrait, Serialize as TlsSerializeTrait};

const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

#[derive(Debug)]
pub struct MlsIdentity {
    credential_with_key: CredentialWithKey,
    signer: SignatureKeyPair,
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

    pub fn key_package(&self, provider: &OpenMlsRustCrypto) -> Result<KeyPackage> {
        let bundle = KeyPackage::builder().build(
            CIPHERSUITE,
            provider,
            &self.signer,
            self.credential_with_key.clone(),
        )?;
        Ok(bundle.key_package().clone())
    }

    pub fn key_package_bytes(&self, provider: &OpenMlsRustCrypto) -> Result<Vec<u8>> {
        Ok(self.key_package(provider)?.tls_serialize_detached()?)
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
        guild_id: GuildId,
        channel_id: ChannelId,
        group_state_bytes: &[u8],
    ) -> Result<()>;
    async fn load_group_state(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Result<Option<Vec<u8>>>;
}

pub struct MlsGroupHandle<S: MlsStore> {
    store: S,
    guild_id: GuildId,
    channel_id: ChannelId,
    provider: OpenMlsRustCrypto,
    identity: MlsIdentity,
    group: Option<MlsGroup>,
}

impl<S: MlsStore> MlsGroupHandle<S> {
    pub fn new(store: S, guild_id: GuildId, channel_id: ChannelId, identity: MlsIdentity) -> Self {
        Self {
            store,
            guild_id,
            channel_id,
            provider: OpenMlsRustCrypto::default(),
            identity,
            group: None,
        }
    }

    pub async fn create_group(&mut self, channel_id: ChannelId) -> Result<()> {
        let group_id = GroupId::from_slice(&channel_id.0.to_le_bytes());
        let config = MlsGroupCreateConfig::builder()
            .ciphersuite(CIPHERSUITE)
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
        let welcome = Welcome::tls_deserialize_exact_bytes(welcome_bytes)?;
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
            .validate(provider.crypto(), ProtocolVersion::default(), CIPHERSUITE)
            .map_err(|e| anyhow!("invalid key package bytes: {e}"))?;
        let signer = &self.identity.signer;
        let group = self
            .group
            .as_mut()
            .ok_or_else(|| anyhow!("group not initialized"))?;
        let (commit, welcome, _group_info) = group.add_members(provider, signer, &[key_package])?;
        let commit_bytes = commit.tls_serialize_detached()?;
        let welcome_bytes = Some(welcome.tls_serialize_detached()?);
        group.merge_pending_commit(provider)?;
        self.persist_group().await?;
        Ok((commit_bytes, welcome_bytes))
    }

    pub async fn remove_member(&mut self, member: LeafNodeIndex) -> Result<Vec<u8>> {
        let provider = &self.provider;
        let signer = &self.identity.signer;
        let group = self
            .group
            .as_mut()
            .ok_or_else(|| anyhow!("group not initialized"))?;
        let (commit, _welcome, _group_info) = group.remove_members(provider, signer, &[member])?;
        let bytes = commit.tls_serialize_detached()?;
        group.merge_pending_commit(provider)?;
        self.persist_group().await?;
        Ok(bytes)
    }

    pub fn encrypt_application(&mut self, plaintext_bytes: &[u8]) -> Result<Vec<u8>> {
        let provider = &self.provider;
        let signer = &self.identity.signer;
        let group = self
            .group
            .as_mut()
            .ok_or_else(|| anyhow!("group not initialized"))?;
        let msg = group.create_message(provider, signer, plaintext_bytes)?;
        Ok(msg.tls_serialize_detached()?)
    }

    pub fn decrypt_application(&mut self, ciphertext_bytes: &[u8]) -> Result<Vec<u8>> {
        let mut ciphertext_bytes = ciphertext_bytes;
        let message_in = MlsMessageIn::tls_deserialize(&mut ciphertext_bytes)?;
        let protocol_message: ProtocolMessage = message_in
            .try_into_protocol_message()
            .map_err(|_| anyhow!("ciphertext did not contain a protocol message"))?;
        let provider = &self.provider;
        let group = self
            .group
            .as_mut()
            .ok_or_else(|| anyhow!("group not initialized"))?;
        let processed = group.process_message(provider, protocol_message)?;

        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app_msg) => Ok(app_msg.into_bytes()),
            ProcessedMessageContent::StagedCommitMessage(staged_commit) => {
                group.merge_staged_commit(provider, *staged_commit)?;
                Ok(Vec::new())
            }
            _ => Err(anyhow!("message was not an application message")),
        }
    }

    pub fn export_secret(&self, label: &str, len: usize) -> Result<Vec<u8>> {
        let group = self
            .group
            .as_ref()
            .ok_or_else(|| anyhow!("group not initialized"))?;
        Ok(group.export_secret(&self.provider, label, &[], len)?)
    }

    async fn persist_group(&self) -> Result<()> {
        if let Some(group) = &self.group {
            // OpenMLS 0.6 does not expose direct TLS serialization for `MlsGroup` itself.
            // Persist group identifier bytes as the stable handle until a dedicated
            // state-export/import API is wired in for full snapshot persistence.
            self.store
                .save_group_state(self.guild_id, self.channel_id, group.group_id().as_slice())
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, sync::Arc};
    use tokio::sync::Mutex;

    #[derive(Default, Clone)]
    struct MemoryStore {
        identities: Arc<Mutex<HashMap<(i64, String), Vec<u8>>>>,
        groups: Arc<Mutex<HashMap<(i64, i64), Vec<u8>>>>,
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
            guild_id: GuildId,
            channel_id: ChannelId,
            group_state_bytes: &[u8],
        ) -> Result<()> {
            self.groups
                .lock()
                .await
                .insert((guild_id.0, channel_id.0), group_state_bytes.to_vec());
            Ok(())
        }

        async fn load_group_state(
            &self,
            guild_id: GuildId,
            channel_id: ChannelId,
        ) -> Result<Option<Vec<u8>>> {
            Ok(self
                .groups
                .lock()
                .await
                .get(&(guild_id.0, channel_id.0))
                .cloned())
        }
    }

    #[tokio::test]
    async fn merge_commit_before_decrypting_next_application() {
        let guild_id = GuildId(1);
        let channel_id = ChannelId(20);

        let alice_identity = MlsIdentity::new_with_name(b"alice".to_vec()).expect("alice identity");
        let bob_identity = MlsIdentity::new_with_name(b"bob".to_vec()).expect("bob identity");
        let charlie_identity =
            MlsIdentity::new_with_name(b"charlie".to_vec()).expect("charlie identity");

        let provider = OpenMlsRustCrypto::default();
        let bob_kp = bob_identity
            .key_package_bytes(&provider)
            .expect("bob key package bytes");
        let charlie_kp = charlie_identity
            .key_package_bytes(&provider)
            .expect("charlie key package bytes");

        let mut alice =
            MlsGroupHandle::new(MemoryStore::default(), guild_id, channel_id, alice_identity);
        alice.create_group(channel_id).await.expect("create group");

        let (commit_to_bob, welcome_for_bob) = alice.add_member(&bob_kp).await.expect("add bob");

        let mut bob =
            MlsGroupHandle::new(MemoryStore::default(), guild_id, channel_id, bob_identity);
        bob.join_group_from_welcome(&welcome_for_bob.expect("welcome bob"))
            .await
            .expect("bob joins");

        let (commit_for_existing, _welcome_for_charlie) =
            alice.add_member(&charlie_kp).await.expect("add charlie");

        let _ = bob
            .decrypt_application(&commit_for_existing)
            .expect("process inbound commit and merge it");

        let ciphertext = alice
            .encrypt_application(b"post-commit message")
            .expect("encrypt app message");
        let plaintext = bob
            .decrypt_application(&ciphertext)
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

        let bob_provider = OpenMlsRustCrypto::default();
        let bob_kp = bob_identity
            .key_package_bytes(&bob_provider)
            .expect("bob key package bytes");

        let mut alice =
            MlsGroupHandle::new(MemoryStore::default(), guild_id, channel_id, alice_identity);
        alice.create_group(channel_id).await.expect("create group");

        let (_commit, welcome) = alice.add_member(&bob_kp).await.expect("add member");
        let welcome = welcome.expect("welcome message");

        let mut bob =
            MlsGroupHandle::new(MemoryStore::default(), guild_id, channel_id, bob_identity);
        bob.join_group_from_welcome(&welcome)
            .await
            .expect("bob joins group");

        let ciphertext = alice
            .encrypt_application(b"hello bob")
            .expect("encrypt app message");

        let plaintext = bob
            .decrypt_application(&ciphertext)
            .expect("decrypt app message");

        assert_eq!(plaintext, b"hello bob");
    }
}
