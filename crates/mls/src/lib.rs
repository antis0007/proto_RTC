use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::{MemoryStorage, RustCrypto};
use openmls_traits::OpenMlsProvider;
use serde::{Deserialize, Serialize};
use shared::domain::{ChannelId, GuildId};
use std::{collections::HashMap, sync::RwLock};
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

#[derive(Default, Debug)]
pub struct PersistentOpenMlsProvider {
    crypto: RustCrypto,
    storage: MemoryStorage,
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializableStorage {
    entries: Vec<(Vec<u8>, Vec<u8>)>,
}

impl PersistentOpenMlsProvider {
    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let snapshot: SerializableStorage = serde_json::from_slice(bytes)
            .context("failed to deserialize OpenMLS storage snapshot")?;
        let values = snapshot.entries.into_iter().collect::<HashMap<_, _>>();
        Ok(Self {
            crypto: RustCrypto::default(),
            storage: MemoryStorage {
                values: RwLock::new(values),
            },
        })
    }

    fn to_bytes(&self) -> Result<Vec<u8>> {
        let entries = self
            .storage
            .values
            .read()
            .map_err(|_| anyhow!("failed to read OpenMLS storage lock"))?
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Ok(serde_json::to_vec(&SerializableStorage { entries })?)
    }
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
    guild_id: GuildId,
    channel_id: ChannelId,
    provider: PersistentOpenMlsProvider,
    identity: MlsIdentity,
    group: Option<MlsGroup>,
}

impl<S: MlsStore> MlsGroupHandle<S> {
    pub fn new(store: S, guild_id: GuildId, channel_id: ChannelId, identity: MlsIdentity) -> Self {
        Self {
            store,
            guild_id,
            channel_id,
            provider: PersistentOpenMlsProvider::default(),
            identity,
            group: None,
        }
    }

    pub async fn load_or_create_group(&mut self) -> Result<()> {
        if let Some(saved_state) = self
            .store
            .load_group_state(self.guild_id, self.channel_id)
            .await?
        {
            self.provider = PersistentOpenMlsProvider::from_bytes(&saved_state)?;
            let group_id = GroupId::from_slice(&self.channel_id.0.to_le_bytes());
            self.group = Some(
                MlsGroup::load(self.provider.storage(), &group_id)
                    .map_err(|e| anyhow!("failed to load persisted mls group: {e}"))?
                    .ok_or_else(|| anyhow!("persisted mls storage missing group state"))?,
            );
            Ok(())
        } else {
            self.create_group(self.channel_id).await
        }
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
            .ok_or_else(|| anyhow!("group not initialized"))?;
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
            .ok_or_else(|| anyhow!("group not initialized"))?;
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
            .ok_or_else(|| anyhow!("group not initialized"))?;
        let processed = group.process_message(provider, protocol_message)?;

        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app_msg) => Ok(app_msg.into_bytes()),
            ProcessedMessageContent::StagedCommitMessage(staged_commit) => {
                group.merge_staged_commit(provider, *staged_commit)?;
                self.persist_group().await?;
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
        self.store
            .save_group_state(self.guild_id, self.channel_id, &self.provider.to_bytes()?)
            .await?;
        Ok(())
    }
}
