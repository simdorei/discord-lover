use super::{ActionError, original_owner_error};
use cdr_app_server::AppServerError;
use std::path::Path;

pub(super) fn failure(
    db: &Path,
    reservation: &cdr_store::archive_fence::Reservation,
    thread: &str,
    error: AppServerError,
) -> ActionError {
    let rejected_before_effect = matches!(
        &error,
        AppServerError::GenerationMismatch { .. }
            | AppServerError::GenerationQuarantined { .. }
            | AppServerError::DeadGenerationFence { .. }
    ) || matches!(&error, AppServerError::Remote { code: -32_600, message, .. }
        if message.contains("already has an active writer"));
    if rejected_before_effect {
        if let Err(release) = cdr_store::archive_fence::release_rejected(db, reservation) {
            return ActionError::Invalid(format!(
                "archive was rejected before effect, but its reservation could not be released and remains protected: {release}. Original error: {error}"
            ));
        }
        return original_owner_error("archive", thread, error);
    }
    ActionError::Invalid(format!(
        "archive attempt for {thread} has an unverified outcome; some conversations may already be archived; its reservation remains protected; do not automatically retry. Original app-server error: {error}"
    ))
}
