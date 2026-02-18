use std::{collections::HashMap, path::Path, sync::Arc};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use mls::{MlsGroupHandle, MlsIdentity, MlsStore};
use shared::domain::{ChannelId, GuildId};
use storage::Storage;
use tokio::sync::Mutex;

use crate::{MlsAddMemberOutcome, MlsSessionManager};

type SessionKey = (GuildId, ChannelId);

pub struct DurableMlsSessionManager {
    store: Storage,
    user_id: i64,
    device_id: String,
    sessions: Mutex<HashMap<SessionKey, MlsGroupHandle<Storage>>>,
    channel_index: Mutex<HashMap<ChannelId, GuildId>>,
}

impl DurableMlsSessionManager {
    pub async fn initialize(
        database_url: &str,
        user_id: i64,
        device_id: impl Into<String>,
    ) -> Result<Arc<Self>> {
        let store = Storage::new(database_url)
            .await
            .with_context(|| format!("failed to initialize MLS storage at '{database_url}'"))?;
        let manager = Arc::new(Self {
            store,
            user_id,
            device_id: device_id.into(),
            sessions: Mutex::new(HashMap::new()),
            channel_index: Mutex::new(HashMap::new()),
        });
        manager.load_or_create_identity().await?;
        Ok(manager)
    }

    fn sqlite_url_from_path(path: &Path) -> String {
        format!("sqlite://{}", path.display())
    }

    async fn load_or_create_identity(&self) -> Result<MlsIdentity> {
        if let Some(identity_bytes) = self
            .store
            .load_identity_keys(self.user_id, &self.device_id)
            .await?
        {
            return MlsIdentity::from_bytes(&identity_bytes);
        }

        let identity = MlsIdentity::new_with_name(
            format!("user:{}:{}", self.user_id, self.device_id).into_bytes(),
        )?;
        self.store
            .save_identity_keys(self.user_id, &self.device_id, &identity.to_bytes()?)
            .await?;
        Ok(identity)
    }

    async fn key_for_channel(&self, channel_id: ChannelId) -> Result<SessionKey> {
        let guild_id = self
            .channel_index
            .lock()
            .await
            .get(&channel_id)
            .copied()
            .ok_or_else(|| {
                anyhow!(
                    "MLS group not opened for channel {}; channel state is uninitialized for this session",
                    channel_id.0
                )
            })?;
        Ok((guild_id, channel_id))
    }

    pub fn sqlite_url_for_gui_data_dir(base_dir: &Path) -> String {
        Self::sqlite_url_from_path(&base_dir.join("mls_client_state.sqlite3"))
    }
}

#[async_trait]
impl MlsSessionManager for DurableMlsSessionManager {
    async fn key_package_bytes(&self, guild_id: GuildId) -> Result<Vec<u8>> {
        let mut sessions = self.sessions.lock().await;
        if let Some(handle) = sessions
            .iter_mut()
            .find_map(|((session_guild_id, _), handle)| {
                (*session_guild_id == guild_id).then_some(handle)
            })
        {
            return handle.key_package_bytes();
        }

        let identity = self.load_or_create_identity().await?;
        let handle = MlsGroupHandle::new(
            self.store.clone(),
            self.user_id,
            self.device_id.clone(),
            guild_id,
            ChannelId(0),
            identity,
        )
        .await?;
        handle.key_package_bytes()
    }

    async fn open_or_create_group(&self, guild_id: GuildId, channel_id: ChannelId) -> Result<()> {
        {
            let mut index = self.channel_index.lock().await;
            if let Some(existing_guild) = index.get(&channel_id) {
                if *existing_guild != guild_id {
                    return Err(anyhow!(
                        "channel {} is already bound to guild {} in this MLS session",
                        channel_id.0,
                        existing_guild.0
                    ));
                }
            }
            index.insert(channel_id, guild_id);
        }
        let key = (guild_id, channel_id);
        if self.sessions.lock().await.contains_key(&key) {
            return Ok(());
        }

        let identity = self.load_or_create_identity().await?;
        let mut handle = MlsGroupHandle::new(
            self.store.clone(),
            self.user_id,
            self.device_id.clone(),
            guild_id,
            channel_id,
            identity,
        )
        .await?;
        handle.load_or_create_group().await?;
        self.sessions.lock().await.insert(key, handle);
        Ok(())
    }

    async fn encrypt_application(
        &self,
        channel_id: ChannelId,
        plaintext: &[u8],
    ) -> Result<Vec<u8>> {
        let key = self.key_for_channel(channel_id).await?;
        let mut sessions = self.sessions.lock().await;
        let handle = sessions.get_mut(&key).ok_or_else(|| {
            anyhow!(
                "MLS session missing for guild {} channel {}",
                key.0 .0,
                key.1 .0
            )
        })?;
        handle.encrypt_application(plaintext)
    }

    async fn decrypt_application(
        &self,
        channel_id: ChannelId,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>> {
        let key = self.key_for_channel(channel_id).await?;
        let mut sessions = self.sessions.lock().await;
        let handle = sessions.get_mut(&key).ok_or_else(|| {
            anyhow!(
                "MLS session missing for guild {} channel {}",
                key.0 .0,
                key.1 .0
            )
        })?;
        handle.decrypt_application(ciphertext).await
    }

    async fn add_member(
        &self,
        channel_id: ChannelId,
        key_package_bytes: &[u8],
    ) -> Result<MlsAddMemberOutcome> {
        let key = self.key_for_channel(channel_id).await?;
        let mut sessions = self.sessions.lock().await;
        let handle = sessions.get_mut(&key).ok_or_else(|| {
            anyhow!(
                "MLS session missing for guild {} channel {}",
                key.0 .0,
                key.1 .0
            )
        })?;
        let (commit, welcome) = handle.add_member(key_package_bytes).await?;
        let welcome_bytes =
            welcome.ok_or_else(|| anyhow!("MLS add_member did not return a welcome"))?;
        Ok(MlsAddMemberOutcome {
            commit_bytes: commit,
            welcome_bytes,
        })
    }

    async fn join_from_welcome(&self, channel_id: ChannelId, welcome_bytes: &[u8]) -> Result<()> {
        let key = self.key_for_channel(channel_id).await?;
        let mut sessions = self.sessions.lock().await;
        let handle = sessions.get_mut(&key).ok_or_else(|| {
            anyhow!(
                "MLS session missing for guild {} channel {}",
                key.0 .0,
                key.1 .0
            )
        })?;
        handle.join_group_from_welcome(welcome_bytes).await
    }

    async fn export_secret(
        &self,
        channel_id: ChannelId,
        label: &str,
        len: usize,
    ) -> Result<Vec<u8>> {
        let key = self.key_for_channel(channel_id).await?;
        let mut sessions = self.sessions.lock().await;
        let handle = sessions.get_mut(&key).ok_or_else(|| {
            anyhow!(
                "MLS session missing for guild {} channel {}",
                key.0 .0,
                key.1 .0
            )
        })?;
        handle.export_secret(label, len)
    }
}

#[cfg(test)]
mod tests {
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
        bob.join_from_welcome(channel_id, &first_welcome.welcome_bytes)
            .await
            .expect("bob joins");

        let bob_after_restart =
            DurableMlsSessionManager::initialize(&database_url, 2, "device-bob")
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
}
