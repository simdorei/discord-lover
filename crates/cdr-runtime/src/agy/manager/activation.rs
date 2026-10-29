use super::{
    AgyConfig, AgyError, AgyRuntime, Arc, Phase, PreparedAgy, ProbeFuture, Resources, Result,
    Settings,
};

pub(super) enum Activation {
    Blocked(Phase, String),
    Prepared(PreparedAgy, u64),
}
#[allow(
    dead_code,
    reason = "unverified production probes cannot manufacture authentication/cutoff evidence"
)]
pub(in crate::agy) enum Evidence {
    Unverified(String),
    Missing,
    Verified { cutoff: u64 },
}
pub(in crate::agy) trait Probe: Send + Sync {
    fn check<'a>(&'a self, config: &'a AgyConfig) -> ProbeFuture<'a>;
}
pub(in crate::agy) struct UnverifiedProbe;
impl Probe for UnverifiedProbe {
    fn check<'a>(&'a self, _config: &'a AgyConfig) -> ProbeFuture<'a> {
        Box::pin(async {
            Ok(Evidence::Unverified("CLI의 안전한 인증 조회와 입력 시작 경계가 아직 실증되지 않았습니다. 로그인 종료·버전 출력만으로 활성화하지 않습니다.".into()))
        })
    }
}
pub(super) async fn prepare(
    resources: Resources,
    settings: Settings,
    worker: crate::agy::blocking::BlockingWorker,
    probe: Arc<dyn Probe>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    provisioning: Arc<std::sync::atomic::AtomicBool>,
) -> Result<Activation> {
    let config = settings.config;
    let validated = config.clone();
    if let Err(error) = worker.call(move || validated.validate()).await {
        return Ok(Activation::Blocked(Phase::SetupRequired, error.to_string()));
    }
    match probe.check(&config).await? {
        Evidence::Missing => Ok(Activation::Blocked(
            Phase::AuthRequired,
            "AGY 로그인이 필요합니다.".into(),
        )),
        Evidence::Unverified(reason) => {
            Ok(Activation::Blocked(Phase::VerificationRequired, reason))
        }
        Evidence::Verified { cutoff } => {
            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                return Err(AgyError::Stopped);
            }
            provisioning.store(true, std::sync::atomic::Ordering::Release);
            let remote: Arc<dyn crate::mirror_sync::MirrorTransport> = Arc::new(
                crate::mirror_sync::DiscordMirrorTransport::new(Arc::clone(&resources.http)),
            );
            #[cfg(test)]
            let remote = resources.remote.unwrap_or(remote);
            let prepared = AgyRuntime::prepare(
                config,
                resources.policy,
                &resources.root,
                resources.mirror_db,
                resources.http,
                resources.admission,
                cancel,
                remote,
            )
            .await?;
            Ok(Activation::Prepared(prepared, cutoff))
        }
    }
}
