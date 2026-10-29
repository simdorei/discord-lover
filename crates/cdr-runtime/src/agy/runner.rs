//! The official CLI owns authentication and tools. This adapter only consumes
//! its documented print/stream-json protocol; it never reads Google tokens.
use super::{AgyError, Result, config::AgyConfig};
use futures_util::FutureExt;
use serde::Deserialize;
use std::future::Future;
use std::path::Path;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    sync::watch,
};

pub(super) mod process;

const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
#[serde(tag = "event")]
enum Event {
    #[serde(rename = "init")]
    Init { conversation_id: String },
    #[serde(rename = "step_update")]
    Step { step_update: Step },
    #[serde(rename = "result")]
    Result { result: Final },
    #[serde(other)]
    Other,
}
#[derive(Deserialize)]
struct Step {
    conversation_id: Option<String>,
    text_delta: Option<String>,
}
#[derive(Deserialize)]
struct Final {
    conversation_id: Option<String>,
    status: Option<String>,
    response: Option<String>,
}

#[derive(Default)]
struct Output {
    conversation: Option<String>,
    text: String,
    result: Option<Final>,
}

impl Output {
    fn consume(&mut self, line: &str, persist: &mut impl FnMut(&str) -> Result<()>) -> Result<()> {
        if line.trim().is_empty() {
            return Ok(());
        }
        if line.len() > MAX_OUTPUT_BYTES {
            return Err(AgyError::Protocol("AGY event exceeds output limit".into()));
        }
        let event: Event = serde_json::from_str(line).map_err(|_| {
            AgyError::Protocol(
                "AGY returned malformed stream-json; check the CLI version and login".into(),
            )
        })?;
        let id = match &event {
            Event::Init { conversation_id } => Some(conversation_id.as_str()),
            Event::Step { step_update } => step_update.conversation_id.as_deref(),
            Event::Result { result } => result.conversation_id.as_deref(),
            Event::Other => None,
        };
        if let Some(id) = id.filter(|id| !id.is_empty()) {
            if self.conversation.as_deref().is_some_and(|old| old != id) {
                return Err(AgyError::Protocol(
                    "AGY conversation identity changed during execution".into(),
                ));
            }
            if self.conversation.is_none() {
                persist(id)?;
                self.conversation = Some(id.to_owned());
            }
        }
        match event {
            Event::Step { step_update } => {
                if let Some(delta) = step_update.text_delta {
                    if self.text.len().saturating_add(delta.len()) > MAX_OUTPUT_BYTES {
                        return Err(AgyError::Protocol(
                            "AGY response exceeds output limit".into(),
                        ));
                    }
                    self.text.push_str(&delta);
                }
            }
            Event::Result { result } => {
                if self.result.is_some() {
                    return Err(AgyError::Protocol(
                        "AGY emitted more than one final result".into(),
                    ));
                }
                self.result = Some(result);
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(self, code: i32, _stderr: &str) -> Result<String> {
        if code != 0 {
            return Err(AgyError::Protocol(format!(
                "AGY exited with code {code}; CLI diagnostics were withheld because they may contain credentials"
            )));
        }
        let result = self.result.ok_or_else(|| {
            AgyError::Protocol(
                "AGY exited without a final result; check login or pending CLI permissions".into(),
            )
        })?;
        if result.status.as_deref() != Some("SUCCESS") {
            return Err(AgyError::Protocol("AGY returned an unsuccessful result; no automatic retry or credential change was performed".into()));
        }
        if self.conversation.is_none() {
            return Err(AgyError::Protocol(
                "AGY completed without a conversation id".into(),
            ));
        }
        let text = result
            .response
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(self.text);
        if text.trim().is_empty() {
            return Err(AgyError::Protocol(
                "AGY completed without response text".into(),
            ));
        }
        Ok(text)
    }
}

pub(super) fn arguments(
    config: &AgyConfig,
    prompt: &str,
    conversation: Option<&str>,
    cwd: &Path,
) -> Vec<String> {
    let mut args = vec![
        "-p".into(),
        prompt.into(),
        "--output-format".into(),
        "stream-json".into(),
        "--add-dir".into(),
        cwd.to_string_lossy().into_owned(),
    ];
    if let Some(id) = conversation {
        args.extend(["--conversation".into(), id.into()]);
    }
    if let Some(model) = &config.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if config.auto_approve {
        args.push("--dangerously-skip-permissions".into());
    }
    args
}

#[allow(
    clippy::too_many_arguments,
    reason = "one CLI request carries persistence, cancellation and owned-process dependencies"
)]
pub(super) async fn execute<Fut: Future<Output = Result<()>> + Send>(
    config: &AgyConfig,
    prompt: &str,
    conversation: Option<&str>,
    cwd: &Path,
    stop: watch::Receiver<bool>,
    processes: &super::processes::Processes,
    request: &str,
    persist: impl FnMut(String) -> Fut + Send,
) -> Result<String> {
    execute_owned(
        &config.executable,
        &arguments(config, prompt, conversation, cwd),
        cwd,
        stop,
        processes,
        request,
        persist,
    )
    .await
}

async fn execute_owned<Fut: Future<Output = Result<()>> + Send>(
    executable: &Path,
    args: &[String],
    cwd: &Path,
    mut stop: watch::Receiver<bool>,
    processes: &super::processes::Processes,
    request: &str,
    mut persist: impl FnMut(String) -> Fut + Send,
) -> Result<String> {
    if *stop.borrow() {
        return Err(AgyError::Stopped);
    }
    let (stdout, stderr) = processes.launch(request, executable, args, cwd)?;
    let mut stdout = BufReader::new(stdout);
    let mut line_buffer = Vec::new();
    let mut stderr = BufReader::new(stderr);
    // Keep a bounded diagnostic prefix but continue draining to avoid pipe deadlock.
    let mut stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        (&mut stderr).take(8192).read_to_end(&mut bytes).await?;
        tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await?;
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&bytes).into_owned())
    });
    let result=std::panic::AssertUnwindSafe(async {
        let mut output=Output::default();
        let mut exited=None;
        let mut exited_at=None;
        let mut poll=tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            tokio::select! {
                biased;
                _=stop.changed()=>return Err(AgyError::Stopped),
                line=bounded_line(&mut stdout,&mut line_buffer)=> {
                    let Some(line)=line? else { break; };
                    let mut identity=None;
                    output.consume(&line,&mut |id| {identity=Some(id.to_owned());Ok(())})?;
                    if let Some(identity)=identity {persist(identity).await?;}
                }
                _=poll.tick()=> {
                    if exited.is_none() && let Some(code)=processes.try_wait(request)? {
                        exited=Some(code); exited_at=Some(tokio::time::Instant::now()); processes.kill(request)?;
                    }
                    if exited_at.is_some_and(|at|at.elapsed()>std::time::Duration::from_secs(3)) {
                        return Err(AgyError::Protocol("AGY stdout did not close after process exit".into()));
                    }
                }
            }
        }
        let code=loop {
            if let Some(code)=exited { break code; }
            if let Some(code)=processes.try_wait(request)? { break code; }
            tokio::select! {
                _=stop.changed()=>return Err(AgyError::Stopped),
                ()=tokio::time::sleep(std::time::Duration::from_millis(50))=>{}
            }
        };
        Ok((output,code))
    }).catch_unwind().await.unwrap_or_else(|_|Err(AgyError::Protocol("AGY stream task panicked".into())));
    // Dropping the Windows Job Object also kills spawned tools, including on an
    // error or cancellation before the first stream event.
    let cleanup = processes.cleanup(request).await;
    let stderr = if let Ok(result) =
        tokio::time::timeout(std::time::Duration::from_secs(3), &mut stderr_task).await
    {
        result.map_err(|_| AgyError::Protocol("AGY stderr reader failed".into()))??
    } else {
        stderr_task.abort();
        let _ = stderr_task.await;
        return Err(AgyError::Protocol(
            "AGY stderr did not close after cleanup".into(),
        ));
    };
    cleanup?;
    let (output, code) = result?;
    output.finish(code, &stderr)
}

async fn bounded_line(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    bytes: &mut Vec<u8>,
) -> Result<Option<String>> {
    let remaining = MAX_OUTPUT_BYTES
        .saturating_add(1)
        .saturating_sub(bytes.len());
    let read = reader
        .take(remaining as u64)
        .read_until(b'\n', bytes)
        .await?;
    if bytes.len() > MAX_OUTPUT_BYTES {
        return Err(AgyError::Protocol("AGY event exceeds output limit".into()));
    }
    if read == 0 && bytes.is_empty() {
        return Ok(None);
    }
    let line = String::from_utf8(std::mem::take(bytes))
        .map_err(|_| AgyError::Protocol("AGY output is not UTF-8".into()))?;
    Ok(Some(line))
}

#[cfg(all(test, windows))]
async fn execute_process(
    executable: &Path,
    args: &[String],
    cwd: &Path,
    stop: watch::Receiver<bool>,
    mut persist: impl FnMut(&str) -> Result<()> + Send,
) -> Result<String> {
    let processes = super::processes::Processes::default();
    execute_owned(executable, args, cwd, stop, &processes, "test", move |id| {
        std::future::ready(persist(&id))
    })
    .await
}

#[cfg(test)]
mod tests;
