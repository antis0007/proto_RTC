use async_trait::async_trait;
use tokio::sync::broadcast;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveKitRoomOptions {
    pub room_name: String,
    pub token: String,
    pub e2ee_key: Vec<u8>,
    pub e2ee_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalTrack {
    Microphone,
    ScreenShare,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteParticipant {
    pub participant_id: String,
    pub identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveKitRoomEvent {
    ParticipantJoined(RemoteParticipant),
    ParticipantLeft { participant_id: String },
}

#[async_trait]
pub trait LiveKitRoomSession: Send + Sync {
    async fn publish_local_track(&self, track: LocalTrack) -> anyhow::Result<()>;
    async fn unpublish_local_tracks(&self) -> anyhow::Result<()>;
    async fn leave(&self) -> anyhow::Result<()>;
    fn supports_screen_share(&self) -> bool;
    fn is_e2ee_enabled(&self) -> bool;
    fn subscribe_events(&self) -> broadcast::Receiver<LiveKitRoomEvent>;
}

#[async_trait]
pub trait LiveKitRoomConnector: Send + Sync {
    async fn connect(
        &self,
        options: LiveKitRoomOptions,
    ) -> anyhow::Result<std::sync::Arc<dyn LiveKitRoomSession>>;
}
