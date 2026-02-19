use crate::api::ApiContext;
use shared::protocol::ServerEvent;
use tokio::sync::broadcast;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) api: ApiContext,
    pub(crate) events: broadcast::Sender<ServerEvent>,
}
