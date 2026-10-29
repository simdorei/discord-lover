//! The explicit tray action launches the official interactive CLI in this profile.
use super::{AgyConfig, AgyError, Result};
#[cfg(windows)]
use super::{Arc, Duration};
#[derive(Clone, Default)]
pub(super) struct Login {
    #[cfg(windows)]
    child: Arc<std::sync::Mutex<Option<cdr_windows_native::WindowProcess>>>,
}
impl Login {
    #[cfg_attr(
        not(windows),
        expect(
            clippy::unused_self,
            reason = "the manager uses the same instance API on every platform"
        )
    )]
    pub fn active(&self) -> bool {
        #[cfg(windows)]
        {
            self.child.lock().map_or(true, |c| c.is_some())
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
    #[cfg_attr(
        not(windows),
        expect(
            clippy::unused_async,
            reason = "Windows awaits the blocking worker; unsupported hosts retain that async API"
        )
    )]
    pub async fn launch(
        &self,
        config: AgyConfig,
        worker: &crate::agy::blocking::BlockingWorker,
    ) -> Result<()> {
        #[cfg(windows)]
        {
            let slot = Arc::clone(&self.child);
            worker
                .call(move || {
                    config.validate()?;
                    if !cdr_windows_native::local_control::interactive_session()? {
                        return Err(AgyError::Invalid(
                            "이 PC의 Windows 대화형 세션에서 로그인해 주세요.".into(),
                        ));
                    }
                    let mut slot = slot.lock().map_err(|_| AgyError::CleanupUnconfirmed)?;
                    if slot.is_some() {
                        return Err(AgyError::Invalid(
                            "AGY 로그인 창이 이미 열려 있습니다.".into(),
                        ));
                    }
                    let environment = std::env::vars().collect();
                    *slot = Some(cdr_windows_native::WindowProcess::launch(
                        &config.executable,
                        &[],
                        &config.projects[0].cwd,
                        &environment,
                    )?);
                    Ok(())
                })
                .await
        }
        #[cfg(not(windows))]
        {
            let _ = (config, worker);
            Err(AgyError::Invalid(
                "AGY 트레이 로그인은 Windows 대화형 세션에서 지원합니다.".into(),
            ))
        }
    }
    /// Nonblocking poll retains the process/job if the whole tree is not yet gone.
    #[cfg_attr(
        not(windows),
        expect(
            clippy::unused_self,
            clippy::unnecessary_wraps,
            reason = "the shared instance API returns Windows process polling errors"
        )
    )]
    pub fn poll(&self, stop: bool) -> Result<bool> {
        #[cfg(windows)]
        {
            let mut slot = self
                .child
                .lock()
                .map_err(|_| AgyError::CleanupUnconfirmed)?;
            let Some(child) = slot.as_mut() else {
                return Ok(true);
            };
            if !stop && child.is_running()? {
                return Ok(false);
            }
            match child.terminate_tree(Duration::ZERO) {
                Ok(()) => {
                    *slot = None;
                    Ok(true)
                }
                Err(cdr_windows_native::NativeError::StopTimeout) => Ok(false),
                Err(error) => Err(error.into()),
            }
        }
        #[cfg(not(windows))]
        {
            let _ = stop;
            Ok(true)
        }
    }
}
