use super::ResidentAppServer;
use crate::idle_release::{
    IdleReleaseJournal,
    gate::{MutationAdmission, MutationPermit},
    held,
};
use crate::{
    AppServerClient, AppServerError, RequestId, ServerRequestOccurrence, extract_thread_id,
};
use serde_json::{Value, json};
use std::sync::Arc;

mod response;
pub(super) use response::{ResponseAttempt, ResponsePermit};

pub(super) enum Prepared {
    Ready(Option<MutationPermit>),
    Completed(Value),
}

impl ResidentAppServer {
    pub fn install_idle_release_journal(
        &self,
        journal: Arc<dyn IdleReleaseJournal>,
    ) -> Result<(), AppServerError> {
        let tracked = journal.tracks_observations();
        self.target_gate.install(journal)?;
        if tracked {
            self.state
                .admit_response(self.generation())?
                .client
                .inner
                .state
                .lock()
                .expect("runtime state lock")
                .idle_ledger_required = true;
        }
        Ok(())
    }

    /// A gap only disqualifies optional release, never ordinary execution.
    pub fn mark_idle_observation_gap(&self) {
        self.target_gate.mark_gap();
        if self
            .target_gate
            .journal()
            .is_some_and(|journal| journal.tracks_observations())
        {
            self.target_gate.hold_unattributed_gap();
        }
        if let Some(journal) = self.target_gate.journal()
            && journal.tracks_observations()
            && let Err(error) =
                journal.record_observation_gap(self.instance_id(), self.generation())
        {
            eprintln!("observation_gap_store_error error={error}; unsealed stream retained");
        }
    }

    pub fn confirm_idle_observation(&self, generation: u64, notification: &crate::Notification) {
        if let Ok(admission) = self.state.admit_response(generation) {
            admission
                .client
                .inner
                .state
                .lock()
                .expect("runtime state lock")
                .confirm_idle_observation(notification);
        }
    }

    pub(super) fn settle_exited_idle_owner(
        &self,
        client: &AppServerClient,
        generation: u64,
    ) -> Result<(), AppServerError> {
        // The exact child slot is removed only after wait() proved exit. A closed
        // pipe, different generation or new instance is not equivalent evidence.
        if client
            .inner
            .state
            .lock()
            .expect("runtime state lock")
            .process_exit_confirmed
            && let Some(journal) = self.target_gate.journal()
        {
            journal.old_child_exited(self.instance_id(), generation)?;
        }
        Ok(())
    }

    pub fn subscription_resume_required(&self, thread: &str) -> Result<bool, AppServerError> {
        self.target_gate
            .journal()
            .map_or(Ok(false), |j| j.resume_required(thread))
    }

    pub(super) async fn prepare_mutation(
        &self,
        method: &str,
        params: &Value,
        generation: u64,
    ) -> Result<Prepared, AppServerError> {
        self.prepare_mutation_checked(method, params, generation, None)
            .await
    }

    pub(super) async fn prepare_mutation_checked(
        &self,
        method: &str,
        params: &Value,
        generation: u64,
        check: Option<super::dispatch::DispatchCheck>,
    ) -> Result<Prepared, AppServerError> {
        if let Some(check) = &check {
            check()?;
        }
        if method == "thread/archive" {
            // Explicit native archive must not resume or wait for idle bookkeeping.
            return Ok(Prepared::Ready(None));
        }
        if matches!(
            method,
            "thread/read"
                | "thread/turns/list"
                | "thread/goal/get"
                | "thread/list"
                | "thread/loaded/list"
                | "model/list"
                | "account/rateLimits/read"
                | "account/usage/read"
                | "thread/start"
        ) {
            // thread/start creates a distinct new ID; it cannot mutate a held existing target.
            return Ok(Prepared::Ready(None));
        }
        if let Some(fence) = &self.dead_generation_fence {
            fence.check_request(generation, method, params)?;
        }
        let target = extract_thread_id(params);
        match self
            .target_gate
            .admit(self.instance_id(), generation, target.clone())?
        {
            MutationAdmission::Ordinary(permit) => Ok(Prepared::Ready(Some(permit))),
            MutationAdmission::Resubscribe(permit, token) => {
                let resume = if method == "thread/resume" {
                    params.clone()
                } else {
                    json!({"threadId":token.thread_id})
                };
                let result = self
                    .resubscribe_managed(permit, token, resume, check)
                    .await?;
                if method == "thread/resume" {
                    return Ok(Prepared::Completed(result));
                }
                match self
                    .target_gate
                    .admit(self.instance_id(), generation, target)?
                {
                    MutationAdmission::Ordinary(permit) => Ok(Prepared::Ready(Some(permit))),
                    MutationAdmission::Resubscribe(_, _) => Err(held(
                        "unexpected second resubscription; target remains held",
                    )),
                }
            }
        }
    }

    pub(super) fn check_actual_mutation(
        &self,
        permit: Option<&MutationPermit>,
        generation: u64,
        method: &str,
        params: &Value,
    ) -> Result<(), AppServerError> {
        if self.generation() != generation {
            return Err(AppServerError::GenerationMismatch {
                expected: generation,
                actual: self.generation(),
            });
        }
        if let Some(permit) = permit {
            permit.preflight()?;
        }
        if let Some(fence) = &self.dead_generation_fence {
            fence.check_request(generation, method, params)?;
        }
        Ok(())
    }

    pub(super) async fn response_permit(
        &self,
        client: &AppServerClient,
        id: &RequestId,
        occurrence: ServerRequestOccurrence,
        generation: u64,
    ) -> Result<ResponsePermit, AppServerError> {
        let original = {
            let state = client.inner.state.lock().expect("runtime state lock");
            state.server_response_candidate(id, occurrence)?.clone()
        };
        let authority = self
            .dead_generation_fence
            .as_ref()
            .map(|fence| fence.response_authority((self.instance_id(), generation), &original))
            .transpose()?
            .flatten();
        match self
            .prepare_mutation("server/response", &original.params, generation)
            .await?
        {
            // Preserve this exact occurrence's params across the target/writer
            // waits. A final check must not silently become an unscoped one.
            Prepared::Ready(permit) => Ok(ResponsePermit {
                permit,
                original,
                authority,
            }),
            Prepared::Completed(_) => Err(held("unexpected response admission")),
        }
    }
}
