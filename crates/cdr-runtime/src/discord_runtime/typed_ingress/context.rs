use std::path::{Path, PathBuf};
use std::sync::Arc;

use cdr_app_server::ResidentAppServer;
use twilight_http::Client;
use twilight_model::id::{Id, marker::ApplicationMarker};

use super::interaction::InteractionResources;
use crate::action_executor::ActionExecutor;
use crate::app_backend::AppServerTurnBackend;
use crate::config::RuntimeConfig;
use crate::message_worker::MessageContext;
use crate::restart_readiness::drain::AdmissionGate;

#[derive(Clone)]
pub(in crate::discord_runtime) struct TypedIngressContext {
    pub(in crate::discord_runtime) config: Arc<RuntimeConfig>,
    pub(in crate::discord_runtime) http: Arc<Client>,
    pub(in crate::discord_runtime) executor: Arc<ActionExecutor<AppServerTurnBackend>>,
    pub(in crate::discord_runtime) server: Arc<ResidentAppServer>,
    pub(super) interaction: InteractionResources,
    pub(in crate::discord_runtime) admission: AdmissionGate,
    pub(in crate::discord_runtime) agy: Option<crate::agy::routing::AgyRouting>,
    attachment_root: PathBuf,
    attachment_client: Arc<reqwest::Client>,
}

impl TypedIngressContext {
    #[allow(
        clippy::too_many_arguments,
        reason = "the constructor explicitly assembles the shared typed-ingress dependencies"
    )]
    pub(in crate::discord_runtime) fn new(
        config: Arc<RuntimeConfig>,
        http: Arc<Client>,
        executor: Arc<ActionExecutor<AppServerTurnBackend>>,
        server: Arc<ResidentAppServer>,
        attachment_root: PathBuf,
        attachment_client: Arc<reqwest::Client>,
        interaction: InteractionResources,
        admission: AdmissionGate,
    ) -> Self {
        Self {
            config,
            http,
            executor,
            server,
            interaction,
            admission,
            agy: None,
            attachment_root,
            attachment_client,
        }
    }

    pub(in crate::discord_runtime) fn with_agy(
        mut self,
        agy: Option<crate::agy::routing::AgyRouting>,
    ) -> Self {
        self.agy = agy;
        self
    }

    pub(in crate::discord_runtime) fn message_context(
        &self,
        application_id: Id<ApplicationMarker>,
    ) -> MessageContext<'_, AppServerTurnBackend> {
        MessageContext::new(
            application_id,
            &self.config,
            &self.executor,
            &self.server,
            Arc::clone(&self.http),
            &self.attachment_root,
            &self.attachment_client,
        )
    }

    pub(super) fn mirror_db(&self) -> &Path {
        self.executor.mirror_db()
    }
}
