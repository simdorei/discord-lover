use crate::component_worker::{BusyComponentError, ComponentWorkerError};

use super::InteractionWorkerError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractionErrorDisposition {
    IgnoreDuplicate,
    LogOnly,
    Report,
}

#[must_use]
pub fn interaction_error_disposition(
    error: &InteractionWorkerError,
) -> InteractionErrorDisposition {
    if matches!(
        error,
        InteractionWorkerError::Component(
            ComponentWorkerError::AlreadyHandled
                | ComponentWorkerError::Busy(BusyComponentError::AlreadyHandled)
        )
    ) {
        InteractionErrorDisposition::IgnoreDuplicate
    } else if matches!(
        error,
        InteractionWorkerError::KnownOutcomeNotification(_)
            | InteractionWorkerError::AsyncAnswerNotice(_)
            | InteractionWorkerError::Component(
                ComponentWorkerError::ActionUnconfirmed
                    | ComponentWorkerError::ActionOutcomeIndeterminate(_)
                    | ComponentWorkerError::Confirmation(_)
                    | ComponentWorkerError::Busy(
                        BusyComponentError::ActionUnconfirmed
                            | BusyComponentError::ActionOutcomeIndeterminate(_)
                            | BusyComponentError::Confirmation(_)
                    )
            )
    ) {
        InteractionErrorDisposition::LogOnly
    } else {
        InteractionErrorDisposition::Report
    }
}
