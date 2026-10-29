use crate::agy::Result;
use std::{path::Path, pin::Pin};
use tokio::io::AsyncRead;

pub(in crate::agy) type Reader = Pin<Box<dyn AsyncRead + Send>>;

#[cfg(windows)]
pub(in crate::agy) struct Child(cdr_windows_native::CapturedWindowProcess);

#[cfg(windows)]
pub(in crate::agy) fn launch(
    exe: &Path,
    args: &[String],
    cwd: &Path,
) -> Result<(Child, Reader, Reader)> {
    let mut child = cdr_windows_native::CapturedWindowProcess::launch(
        exe,
        args,
        cwd,
        &std::env::vars().collect(),
    )?;
    let stdout = child.take_stdout().expect("captured process has stdout");
    let stderr = child.take_stderr().expect("captured process has stderr");
    Ok((
        Child(child),
        Box::pin(tokio::fs::File::from_std(stdout)),
        Box::pin(tokio::fs::File::from_std(stderr)),
    ))
}

#[cfg(windows)]
impl Child {
    pub(in crate::agy) fn try_wait(&mut self) -> Result<Option<i32>> {
        Ok(self.0.try_wait()?)
    }
    pub(in crate::agy) fn kill(&mut self) -> Result<()> {
        Ok(self.0.start_kill()?)
    }
    pub(in crate::agy) fn terminate_tree_now(&mut self) -> Result<bool> {
        match self.0.terminate_tree(std::time::Duration::ZERO) {
            Ok(()) => Ok(true),
            Err(cdr_windows_native::NativeError::StopTimeout) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(not(windows))]
pub(in crate::agy) struct Child(tokio::process::Child);

#[cfg(not(windows))]
pub(in crate::agy) fn launch(
    exe: &Path,
    args: &[String],
    cwd: &Path,
) -> Result<(Child, Reader, Reader)> {
    use std::process::Stdio;
    let mut child = tokio::process::Command::new(exe)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    Ok((Child(child), Box::pin(stdout), Box::pin(stderr)))
}

#[cfg(not(windows))]
impl Child {
    pub(in crate::agy) fn try_wait(&mut self) -> Result<Option<i32>> {
        Ok(self.0.try_wait()?.map(|s| s.code().unwrap_or(-1)))
    }
    pub(in crate::agy) fn kill(&mut self) -> Result<()> {
        Ok(self.0.start_kill()?)
    }
    pub(in crate::agy) fn terminate_tree_now(&mut self) -> Result<bool> {
        self.0.start_kill()?;
        Ok(self.0.try_wait()?.is_some())
    }
}
