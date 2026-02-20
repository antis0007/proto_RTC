use serde::{Deserialize, Serialize};

macro_rules! id_newtype {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub i64);
    };
}

id_newtype!(UserId);
id_newtype!(DeviceId);
id_newtype!(GuildId);
id_newtype!(ChannelId);
id_newtype!(MessageId);
id_newtype!(FileId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Text,
    Voice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Mod,
    Member,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceLinkState {
    Pending,
    Linked,
    Revoked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceSummary {
    pub device_id: DeviceId,
    pub user_id: UserId,
    pub device_name: String,
    pub device_public_identity: String,
    pub is_revoked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkedDeviceSummary {
    pub device: DeviceSummary,
    pub link_state: DeviceLinkState,
}
