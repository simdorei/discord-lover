use super::{ActionError, ActionExecutor, ActionResult, immediate};
use crate::{
    mirror_sync::{DiscordMirrorTransport, MirrorSynchronizer, MirrorTransport},
    queue_runner::TurnBackend,
};
use std::sync::Arc;

impl<B: TurnBackend> ActionExecutor<B> {
    pub(super) async fn inspect_mirror(
        &self,
        channel: u64,
        limit: Option<u32>,
        list: bool,
    ) -> Result<ActionResult, ActionError> {
        let sync = self.mirror_sync.get().ok_or(ActionError::Unsupported(
            "mirror inspection transport is not configured; remote state was not checked",
        ))?;
        Ok(immediate(sync.inspect(channel, limit, list).await?))
    }

    pub fn configure_mirror_sync(
        &self,
        http: Arc<twilight_http::Client>,
        guild: Option<u64>,
    ) -> Result<(), ActionError> {
        self.set_mirror_transport(Arc::new(DiscordMirrorTransport::new(http)), guild)
    }

    pub fn set_mirror_transport(
        &self,
        remote: Arc<dyn MirrorTransport>,
        guild: Option<u64>,
    ) -> Result<(), ActionError> {
        self.mirror_sync
            .set(MirrorSynchronizer::new(
                self.state_db.clone(),
                self.mirror_db.clone(),
                remote,
                guild,
            ))
            .map_err(|_| ActionError::Invalid("mirror sync is already configured".into()))
    }

    pub(crate) fn configure_mirror_with_ownership(
        &self,
        http: Arc<twilight_http::Client>,
        guild: Option<u64>,
        ownership: Arc<crate::agy::ownership::AgyOwnership>,
    ) -> Result<(), ActionError> {
        self.mirror_sync
            .set(
                MirrorSynchronizer::new(
                    self.state_db.clone(),
                    self.mirror_db.clone(),
                    Arc::new(DiscordMirrorTransport::new(http)),
                    guild,
                )
                .with_ownership(ownership),
            )
            .map_err(|_| ActionError::Invalid("mirror sync is already configured".into()))
    }

    pub(super) async fn bridge_sync(
        &self,
        channel: u64,
        limit: Option<i64>,
    ) -> Result<ActionResult, ActionError> {
        let sync = self.mirror_sync.get().ok_or(ActionError::Unsupported(
            "mirror sync transport is not configured",
        ))?;
        Ok(immediate(sync.sync(channel, limit).await?.to_string()))
    }
}
