use std::fs::{self, OpenOptions};
use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use cdr_app_server::{AppServerConfig, AppServerError, ResidentAppServer};
use serde_json::{Value, json};

const FIXTURE_ENV: &str = "CDR_DEAD_GENERATION_FIXTURE";
const METHODS_ENV: &str = "CDR_DEAD_GENERATION_METHODS";

pub fn config(methods: &Path) -> AppServerConfig {
    let mut config = AppServerConfig::new(std::env::current_exe().unwrap());
    config.arguments = vec![
        "--exact".into(),
        "support::fake_app_server_fixture".into(),
        "--ignored".into(),
        "--nocapture".into(),
    ];
    config.environment.insert(FIXTURE_ENV.into(), "1".into());
    config
        .environment
        .insert(METHODS_ENV.into(), methods.to_string_lossy().into_owned());
    config
}

pub async fn kill_by_protocol(server: &ResidentAppServer, method: &str) {
    let generation = server.generation();
    let _ = server
        .request(method, json!({}), Duration::from_secs(2), Some(generation))
        .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = server.lifecycle_snapshot().await;
            if !snapshot.healthy && snapshot.restart_pending {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fixture process death was observed");
}

pub async fn restart_dead(server: &ResidentAppServer) -> Result<bool, AppServerError> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match server.force_restart_if_quiescent().await {
                Ok(false) => tokio::task::yield_now().await,
                result => return result,
            }
        }
    })
    .await
    .expect("dead fixture did not become OS-exit eligible for replacement")
}

pub fn methods(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
#[ignore = "spawned JSON-RPC app-server fixture"]
fn fake_app_server_fixture() {
    if std::env::var_os(FIXTURE_ENV).is_none() {
        return;
    }
    let methods = std::env::var_os(METHODS_ENV).unwrap();
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(methods)
        .unwrap();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let message: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let method = message["method"].as_str().unwrap_or_default();
        let target = message["params"]["threadId"].as_str().unwrap_or_default();
        writeln!(log, "{method} {target}").unwrap();
        log.flush().unwrap();
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let result = match method {
            "initialize" => json!({"userAgent":"dead-fixture/1"}),
            "test/die-active" | "test/active" => {
                send(
                    &mut stdout,
                    &json!({"method":"turn/started","params":{
                    "threadId":"held-thread","turn":{"id":"old-turn"}}}),
                );
                send(
                    &mut stdout,
                    &json!({"id":"private-request-id","method":"item/tool/requestUserInput",
                    "params":{"threadId":"held-thread","turnId":"old-turn","privateFixture":"do-not-emit"}}),
                );
                json!({})
            }
            "test/hang" => continue,
            "thread/read" => json!({"thread":{"id":target,"turns":[],"status":{"type":"idle"}}}),
            "thread/turns/list" => {
                assert_eq!(message["params"]["itemsView"], "notLoaded");
                assert_eq!(message["params"]["sortDirection"], "desc");
                assert_eq!(message["params"]["limit"], 100);
                assert!(message["params"]["cursor"].is_null());
                json!({"data":[],"nextCursor":null})
            }
            "thread/resume" => json!({"thread":{"id":target}}),
            "thread/fork" => json!({"thread":{"id":format!("{target}-fork")}}),
            "turn/start" => json!({"turn":{"id":"new-turn","status":"inProgress"}}),
            _ => json!({}),
        };
        send(&mut stdout, &json!({"id":id,"result":result}));
        if method.starts_with("test/die-") {
            return;
        }
    }
}

fn send(writer: &mut impl Write, value: &Value) {
    serde_json::to_writer(&mut *writer, value).unwrap();
    writeln!(writer).unwrap();
    writer.flush().unwrap();
}
