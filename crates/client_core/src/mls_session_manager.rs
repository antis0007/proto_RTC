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
        Self::ensure_sqlite_parent_dirs(database_url)
            .with_context(|| format!("failed to prepare parent directories for '{database_url}'"))?;

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

        // Ensure identity exists up front (also creates identity rows after a wipe).
        manager.load_or_create_identity().await?;
        Ok(manager)
    }

    fn sqlite_url_from_path(path: &Path) -> String {
        format!("sqlite://{}", path.display())
    }

    fn ensure_sqlite_parent_dirs(database_url: &str) -> Result<()> {
        // Handles file URLs like:
        //   sqlite://C:\...\mls_client_state.sqlite3
        //   sqlite:///home/user/.../mls_client_state.sqlite3
        // Ignores memory/in-memory URLs.
        if !database_url.starts_with("sqlite://") {
            return Ok(());
        }

        let raw = &database_url["sqlite://".len()..];
        if raw.is_empty() || raw == ":memory:" {
            return Ok(());
        }

        let db_path = Path::new(raw);

        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("could not create parent directory '{}'", parent.display())
                })?;
            }
        }

        Ok(())
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

    pub async fn reset_channel_group_state(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Result<bool> {
        {
            let mut sessions = self.sessions.lock().await;
            sessions.remove(&(guild_id, channel_id));
        }
        {
            let mut index = self.channel_index.lock().await;
            index.remove(&channel_id);
        }

        self.store
            .clear_mls_group_state(self.user_id, &self.device_id, guild_id, channel_id)
            .await
    }

    pub async fn reset_all_group_states_for_device(&self) -> Result<u64> {
        {
            let mut sessions = self.sessions.lock().await;
            sessions.clear();
        }
        {
            let mut index = self.channel_index.lock().await;
            index.clear();
        }

        self.store
            .clear_all_mls_group_states_for_device(self.user_id, &self.device_id)
            .await
    }

    pub async fn reset_all_mls_state_for_device(&self) -> Result<(u64, u64)> {
        {
            let mut sessions = self.sessions.lock().await;
            sessions.clear();
        }
        {
            let mut index = self.channel_index.lock().await;
            index.clear();
        }

        self.store
            .clear_all_mls_state_for_device(self.user_id, &self.device_id)
            .await
    }
}

#[async_trait]
impl MlsSessionManager for DurableMlsSessionManager {
    async fn key_package_bytes(&self, guild_id: GuildId) -> Result<Vec<u8>> {
        let mut sessions = self.sessions.lock().await;
        if let Some(existing_handle) = sessions
            .iter_mut()
            .find_map(|((session_guild_id, _), handle)| {
                (*session_guild_id == guild_id).then_some(handle)
            })
        {
            return existing_handle.key_package_bytes().await;
        }
        drop(sessions);

        let identity = self.load_or_create_identity().await?;
        let mut handle = MlsGroupHandle::new(
            self.store.clone(),
            self.user_id,
            self.device_id.clone(),
            guild_id,
            ChannelId(0),
            identity,
        )
        .await?;

        handle.key_package_bytes().await
    }

    async fn has_persisted_group_state(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Result<bool> {
        Ok(self
            .store
            .load_group_state(self.user_id, &self.device_id, guild_id, channel_id)
            .await?
            .is_some())
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

        {
            let sessions = self.sessions.lock().await;
            if sessions.contains_key(&key) {
                return Ok(());
            }
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

        let mut sessions = self.sessions.lock().await;
        sessions.entry(key).or_insert(handle);
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

    async fn group_contains_key_package_identity(
        &self,
        channel_id: ChannelId,
        key_package_bytes: &[u8],
    ) -> Result<bool> {
        let key = self.key_for_channel(channel_id).await?;
        let mut sessions = self.sessions.lock().await;
        let handle = sessions.get_mut(&key).ok_or_else(|| {
            anyhow!(
                "MLS session missing for guild {} channel {}",
                key.0 .0,
                key.1 .0
            )
        })?;
        handle.group_contains_key_package_identity(key_package_bytes)
    }

    async fn join_from_welcome(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        welcome_bytes: &[u8],
    ) -> Result<()> {
        {
            let mut index = self.channel_index.lock().await;
            index.insert(channel_id, guild_id);
        }

        let key = (guild_id, channel_id);

        // If a handle already exists, try to join/apply welcome idempotently on it.
        {
            let mut sessions = self.sessions.lock().await;
            if let Some(existing) = sessions.get_mut(&key) {
                existing.join_group_from_welcome(welcome_bytes).await?;
                return Ok(());
            }
        }

        let identity = self.load_or_create_identity().await?;
        let mut handle = MlsGroupHandle::open_for_join(
            self.store.clone(),
            self.user_id,
            self.device_id.clone(),
            guild_id,
            channel_id,
            identity,
        )
        .await?;

        handle.join_group_from_welcome(welcome_bytes).await?;

        let mut sessions = self.sessions.lock().await;
        sessions.entry(key).or_insert(handle);
        Ok(())
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

    async fn reset_channel_group_state(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Result<bool> {
        DurableMlsSessionManager::reset_channel_group_state(self, guild_id, channel_id).await
    }
}

#[cfg(test)]
#[path = "tests/mls_session_manager_tests.rs"]
mod tests;
