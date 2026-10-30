use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use cdr_discord::interaction::RoutedWork;
use cdr_discord::interaction_access::InteractionAccessPolicy;
use cdr_store::claims::BusyChoice;
use thiserror::Error;
use tokio::{sync::mpsc, time::Duration};
use twilight_model::{
    http::interaction::InteractionResponse,
    id::{
        Id,
        marker::{ApplicationMarker, ChannelMarker, InteractionMarker, MessageMarker, UserMarker},
    },
};

mod admission;
mod async_modal;
mod autocomplete;
mod claims;
mod custody;
pub mod delivery_identity;
mod dispatch_canonical;
mod dispatch_flow;
mod dispatch_queue;
mod dispatch_response;
#[cfg(test)]
mod new_handoff_tests;
mod response;
mod settings_admission;
mod transport;

pub use autocomplete::AutocompleteCatalog;
pub use claims::InteractionClaimCache;

use crate::restart_readiness::drain::{AdmissionGate, AdmissionPermit, DrainGateError};

pub type BoxDiscordFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;
pub const INTERACTION_ACK_BUDGET: Duration = Duration::from_millis(2_500);

pub trait InteractionTransport: Send + Sync + 'static {
    fn acknowledge<'a>(
        &'a self,
        id: Id<InteractionMarker>,
        token: &'a str,
        response: &'a InteractionResponse,
    ) -> BoxDiscordFuture<'a, ()>;
    fn update<'a>(&'a self, token: &'a str, content: &'a str) -> BoxDiscordFuture<'a, ()>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct InboundInteractionWork {
    pub application_id: Id<ApplicationMarker>,
    pub interaction_id: Id<InteractionMarker>,
    pub channel_id: Id<ChannelMarker>,
    pub user_id: Id<UserMarker>,
    pub source_message_id: Option<Id<MessageMarker>>,
    pub interaction_token: String,
    pub work: RoutedWork,
    pub processing_mode: InteractionProcessingMode,
    pub custody_database: PathBuf,
    pub custody_ingress_id: String,
    pub authorized_busy_choice: Option<BusyChoice>,
    pub(crate) admission_permit: Option<AdmissionPermit>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractionProcessingMode {
    Execute,
    ConfirmationOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchOutcome {
    Queued,
    RespondedWithoutWork,
    Duplicate,
    DuplicatePending,
    DeadlineExceeded,
    QueueFull,
    Stopping,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum DiscordDispatchError {
    #[error("failed to acknowledge Discord interaction: {0}")]
    Acknowledge(String),
    #[error("failed to update Discord interaction response: {0}")]
    Update(String),
    #[error("interaction claim cache is full with acknowledgements still in flight")]
    ClaimCacheSaturated,
    #[error("interaction claim state changed before acknowledgement could be committed")]
    ClaimState,
    #[error("durable interaction custody failed: {0}")]
    Custody(String),
    #[error(transparent)]
    Admission(#[from] DrainGateError),
}

pub struct InteractionDispatcher<T: InteractionTransport> {
    transport: Arc<T>,
    policy: Arc<InteractionAccessPolicy>,
    qa_enabled: bool,
    work: mpsc::Sender<InboundInteractionWork>,
    autocomplete: Arc<AutocompleteCatalog>,
    claims: InteractionClaimCache,
    admission: Option<AdmissionGate>,
    ingress_db: PathBuf,
    settings_resolver: Option<crate::settings_binding::SettingsTargetResolver>,
}

impl<T: InteractionTransport> InteractionDispatcher<T> {
    #[must_use]
    pub fn new(
        transport: Arc<T>,
        policy: impl Into<Arc<InteractionAccessPolicy>>,
        qa_enabled: bool,
        work: mpsc::Sender<InboundInteractionWork>,
        ingress_db: impl Into<PathBuf>,
    ) -> Self {
        Self {
            transport,
            policy: policy.into(),
            qa_enabled,
            work,
            autocomplete: Arc::new(AutocompleteCatalog::default()),
            claims: InteractionClaimCache::default(),
            admission: None,
            ingress_db: ingress_db.into(),
            settings_resolver: None,
        }
    }

    #[must_use]
    pub fn with_settings_resolver(
        mut self,
        resolver: crate::settings_binding::SettingsTargetResolver,
    ) -> Self {
        self.settings_resolver = Some(resolver);
        self
    }

    #[must_use]
    pub fn with_autocomplete_catalog(
        mut self,
        catalog: impl Into<Arc<AutocompleteCatalog>>,
    ) -> Self {
        self.autocomplete = catalog.into();
        self
    }

    #[must_use]
    pub fn with_claim_cache(mut self, claims: InteractionClaimCache) -> Self {
        self.claims = claims;
        self
    }

    #[must_use]
    pub fn with_admission_gate(mut self, admission: AdmissionGate) -> Self {
        self.admission = Some(admission);
        self
    }
}
