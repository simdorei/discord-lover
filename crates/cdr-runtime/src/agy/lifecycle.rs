//! One generation owns activation and execution. Unknown outcomes never mean Off.
use super::{AgyError, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Phase {
    Off,
    Checking,
    SetupRequired,
    AuthRequired,
    VerificationRequired,
    Provisioning,
    Ready,
    Draining,
    FaultedCleaning,
    FaultedBlocked,
    Faulted,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub runtime_uuid: String,
    pub sequence: u64,
    pub generation: u64,
    pub config_revision: u64,
    pub enabled: bool,
    pub phase: Phase,
    pub active: usize,
    pub detail: String,
}
impl Status {
    pub(super) fn new(runtime_uuid: String) -> Self {
        Self {
            runtime_uuid,
            sequence: 0,
            generation: 0,
            config_revision: 0,
            enabled: false,
            phase: Phase::Off,
            active: 0,
            detail: String::new(),
        }
    }
    pub(super) fn transition(&mut self, phase: Phase, detail: impl Into<String>) {
        self.phase = phase;
        self.detail = detail.into();
        self.sequence = self.sequence.saturating_add(1);
    }
    pub(super) fn start(&mut self, quiescent: bool) -> Result<bool> {
        if matches!(self.phase, Phase::Checking | Phase::Provisioning) {
            return Ok(false);
        }
        if !quiescent
            || !matches!(
                self.phase,
                Phase::Off
                    | Phase::SetupRequired
                    | Phase::AuthRequired
                    | Phase::VerificationRequired
                    | Phase::Faulted
            )
        {
            return Err(AgyError::Invalid(
                "이전 AGY 작업 정리가 끝나기 전에는 켤 수 없습니다.".into(),
            ));
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| AgyError::Invalid("AGY generation exhausted".into()))?;
        self.transition(Phase::Checking, "AGY 설정과 인증 확인 중");
        Ok(true)
    }
    pub(super) fn can_configure(&self, quiescent: bool) -> bool {
        quiescent
            && matches!(
                self.phase,
                Phase::Off
                    | Phase::SetupRequired
                    | Phase::AuthRequired
                    | Phase::VerificationRequired
                    | Phase::Faulted
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn draining_and_unconfirmed_cleanup_cannot_reactivate_or_change_configuration() {
        let mut status = Status::new("test-runtime".into());
        assert!(status.start(true).unwrap());
        assert!(!status.start(true).unwrap());
        for phase in [
            Phase::Draining,
            Phase::FaultedCleaning,
            Phase::FaultedBlocked,
            Phase::Ready,
        ] {
            status.transition(phase, "");
            assert!(status.start(true).is_err());
            assert!(!status.can_configure(true));
        }
        status.transition(Phase::Off, "");
        assert!(status.start(false).is_err());
        assert!(status.start(true).unwrap());
    }
}
