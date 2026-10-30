//! Isolated OS-process fixture. Never included in the product runtime.
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, ExitCode},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn publish(path: &str, text: String) {
    let next = format!("{path}.fixture-next");
    fs::write(&next, text).unwrap();
    // Match the production heartbeat's atomic publication. An in-place rewrite
    // can transiently look empty to the watchdog and invalidates the fixture.
    while fs::rename(&next, path).is_err() {
        thread::sleep(Duration::from_millis(5));
    }
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|v| v == "--admin") {
        return ExitCode::from(
            Command::new(std::env::var_os("CDR_UPGRADE_REAL_RUNTIME").unwrap())
                .args(args)
                .status()
                .unwrap()
                .code()
                .unwrap_or(1) as u8,
        );
    }
    if args.first().is_some_and(|v| v == "--restart-readiness") {
        if Path::new("fail-readiness").exists() {
            eprintln!("fixture readiness rejection");
            return ExitCode::FAILURE;
        }
        println!("restart_readiness_ok");
        return ExitCode::SUCCESS;
    }
    writeln!(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("fixture-starts.log")
            .unwrap(),
        "{}",
        std::process::id()
    )
    .unwrap();
    if Path::new("fail-start").exists() {
        return ExitCode::FAILURE;
    }
    let exclusive = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(".codex_discord_rust.exclusive.lock")
        .unwrap();
    if exclusive.try_lock().is_err() {
        return ExitCode::FAILURE;
    }
    let pid = std::process::id();
    publish(".codex_discord_rust.runtime.lock", format!("pid={pid}\n"));
    publish(
        ".codex_discord_rust.drain.identity",
        format!("version=1\nruntime_id=fixture-{pid}\npid={pid}\nstate=open\n"),
    );
    let mut recorded = false;
    loop {
        if Path::new("fixture-exit").exists() || Path::new(".codex_discord_rust.stop").exists() {
            break;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        publish(
            ".codex_discord_rust.heartbeat",
            format!("pid={pid}\nupdated_at={now}\n"),
        );
        if !recorded
            && !Path::new("busy").exists()
            && let Ok(prepare) = fs::read_to_string(".codex_discord_rust.drain.prepare")
        {
            publish(".codex_discord_rust.drain.ack", prepare + "state=sealed\n");
            recorded = true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    for path in [
        ".codex_discord_rust.runtime.lock",
        ".codex_discord_rust.heartbeat",
        ".codex_discord_rust.drain.identity",
    ] {
        let _ = fs::remove_file(path);
    }
    ExitCode::SUCCESS
}
