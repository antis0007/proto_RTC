use std::{collections::HashMap, path::Path, sync::Arc};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use mls::{MlsGroupHandle, MlsIdentity};
use shared::domain::{ChannelId, GuildId};
use storage::Storage;
use tokio::sync::Mutex;

use crate::MlsSessionManager;

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
        // Identity persistence is delegated to OpenMLS' internal storage snapshot.
        // We keep a stable credential label for fresh group creation.
        MlsIdentity::new_with_name(
            format!("user:{}:{}", self.user_id, self.device_id).into_bytes(),
        )
    }

    async fn key_for_channel(&self, channel_id: ChannelId) -> Result<SessionKey> {
        let guild_id = self
            .channel_index
            .lock()
            .await
            .get(&channel_id)
            .copied()
            .ok_or_else(|| anyhow!("MLS group not opened for channel {}", channel_id.0))?;
        Ok((guild_id, channel_id))
    }

    pub fn sqlite_url_for_gui_data_dir(base_dir: &Path) -> String {
        Self::sqlite_url_from_path(&base_dir.join("mls_client_state.sqlite3"))
    }
}

#[async_trait]
impl MlsSessionManager for DurableMlsSessionManager {
    async fn open_or_create_group(&self, guild_id: GuildId, channel_id: ChannelId) -> Result<()> {
        self.channel_index.lock().await.insert(channel_id, guild_id);
        let key = (guild_id, channel_id);
        if self.sessions.lock().await.contains_key(&key) {
            return Ok(());
        }

        let identity = self.load_or_create_identity().await?;
        let mut handle = MlsGroupHandle::new(self.store.clone(), guild_id, channel_id, identity);
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

    async fn add_member(&self, channel_id: ChannelId, key_package_bytes: &[u8]) -> Result<Vec<u8>> {
        let key = self.key_for_channel(channel_id).await?;
        let mut sessions = self.sessions.lock().await;
        let handle = sessions.get_mut(&key).ok_or_else(|| {
            anyhow!(
                "MLS session missing for guild {} channel {}",
                key.0 .0,
                key.1 .0
            )
        })?;
        let (_commit, welcome) = handle.add_member(key_package_bytes).await?;
        welcome.ok_or_else(|| anyhow!("MLS add_member did not return a welcome"))
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
