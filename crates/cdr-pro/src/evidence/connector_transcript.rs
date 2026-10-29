use std::path::Path;

use serde_json::{Map, Value, json};
use url::Url;

use super::common::{
    embedded_objects, payload_turn_id, probe_integrity_ok, response_payload, transcript_paths,
};
use super::{EvidenceError, Result};

pub const CONNECTOR_PROTOCOL: &str = "ask-chatgpt-pro-connector-control-v1";
pub const CONNECTOR_NAME: &str = "Simdorei Local Project Oauth";
pub const CONNECTOR_PATH: &str = "/plugins/plugin_asdk_app_6a6ae90be0a08191b877eddba93b631c";
pub const CONNECTOR_PROBE_SHA256: &str =
    "5e61fbefe46be33a0359c4e3acf51ac6fea19cd9cc83b257978d55c5c151b62d";
const PROBE_PATH: &str = "skills/ask-chatgpt-pro/scripts/pro_connector_control.mjs";

pub struct TranscriptEvidence {
    pub trusted_attempt_seen: bool,
    pub evidence: Option<Map<String, Value>>,
}

pub fn canonical_connector_inner_probe_code(plugin_root: &Path) -> Result<String> {
    let path = plugin_root.join(PROBE_PATH);
    let uri = Url::from_file_path(&path)
        .map_err(|()| EvidenceError::InvalidFilePath(path.display().to_string()))?;
    let encoded = serde_json::to_string(uri.as_str())
        .map_err(|error| EvidenceError::InvalidFilePath(format!("{}: {error}", path.display())))?;
    Ok(format!(
        "nodeRepl.write(JSON.stringify(await (await import({encoded})).prepareProConnector({{ proConversationTab }})));"
    ))
}

pub fn canonical_connector_probe_code(plugin_root: &Path) -> Result<String> {
    canonical_outer(plugin_root, "connectorControlResult")
}

pub fn canonical_connector_retry_probe_code(plugin_root: &Path) -> Result<String> {
    canonical_outer(plugin_root, "connectorControlRetryResult")
}

fn canonical_outer(plugin_root: &Path, identifier: &str) -> Result<String> {
    let input = serde_json::to_string(&json!({
        "code": canonical_connector_inner_probe_code(plugin_root)?,
        "title": "Prepare Pro OAuth connector",
    }))
    .map_err(|error| EvidenceError::InvalidFilePath(error.to_string()))?;
    Ok(format!(
        "const {identifier} = await tools.mcp__node_repl__js({input});\nfor (const item of {identifier}.content ?? []) {{\n  if (item.type === \"text\") text(item.text);\n}}"
    ))
}

pub fn read_connector_transcript(
    session_id: &str,
    turn_id: &str,
    codex_home: &Path,
    plugin_root: &Path,
) -> TranscriptEvidence {
    if session_id.is_empty()
        || turn_id.is_empty()
        || !probe_integrity_ok(plugin_root, Path::new(PROBE_PATH), CONNECTOR_PROBE_SHA256)
    {
        return TranscriptEvidence {
            trusted_attempt_seen: false,
            evidence: None,
        };
    }
    let Ok(inner) = canonical_connector_inner_probe_code(plugin_root) else {
        return empty_evidence();
    };
    let Ok(primary) = canonical_connector_probe_code(plugin_root) else {
        return empty_evidence();
    };
    let Ok(retry) = canonical_connector_retry_probe_code(plugin_root) else {
        return empty_evidence();
    };
    let outer = [primary, retry];
    let mut trusted_attempt_seen = false;
    let mut latest_call_id = None;
    let mut latest_evidence = None;
    let mut output_count = 0;
    for path in transcript_paths(codex_home, session_id) {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in content.lines() {
            let Some(payload) = response_payload(line) else {
                continue;
            };
            if payload_turn_id(&payload) != Some(turn_id) {
                continue;
            }
            let Some(call_id) = payload.get("call_id").and_then(Value::as_str) else {
                continue;
            };
            match payload.get("type").and_then(Value::as_str) {
                Some("custom_tool_call") if trusted_call(&payload, &outer, &inner) => {
                    trusted_attempt_seen = true;
                    latest_call_id = Some(call_id.to_owned());
                    latest_evidence = None;
                    output_count = 0;
                }
                Some("custom_tool_call_output") if latest_call_id.as_deref() == Some(call_id) => {
                    output_count += 1;
                    latest_evidence = if output_count == 1 {
                        payload.get("output").and_then(single_valid_evidence)
                    } else {
                        None
                    };
                }
                _ => {}
            }
        }
    }
    TranscriptEvidence {
        trusted_attempt_seen,
        evidence: (output_count == 1).then_some(latest_evidence).flatten(),
    }
}

fn empty_evidence() -> TranscriptEvidence {
    TranscriptEvidence {
        trusted_attempt_seen: false,
        evidence: None,
    }
}

fn trusted_call(payload: &Map<String, Value>, outer: &[String; 2], inner: &str) -> bool {
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return false;
    };
    let Some(code) = payload.get("input").and_then(Value::as_str) else {
        return false;
    };
    let expected: Vec<&str> = match name {
        "exec" | "functions.exec" => outer.iter().map(String::as_str).collect(),
        "js" | "mcp__node_repl__js" | "node_repl.js" => vec![inner],
        _ => return false,
    };
    expected
        .into_iter()
        .any(|item| code == item || code == format!("{item}\n") || code == format!("{item}\r\n"))
}

fn single_valid_evidence(raw: &Value) -> Option<Map<String, Value>> {
    let candidates = embedded_objects(raw)
        .into_iter()
        .filter(valid_connector_evidence)
        .collect::<Vec<_>>();
    (candidates.len() == 1).then(|| candidates[0].clone())
}

pub fn valid_connector_evidence(values: &Map<String, Value>) -> bool {
    if values.get("protocol").and_then(Value::as_str) != Some(CONNECTOR_PROTOCOL)
        || values.get("browser_type").and_then(Value::as_str) != Some("chrome")
        || values.get("connector_name").and_then(Value::as_str) != Some(CONNECTOR_NAME)
        || values.get("connector_path").and_then(Value::as_str) != Some(CONNECTOR_PATH)
    {
        return false;
    }
    let verified = values.get("status").and_then(Value::as_str) == Some("verified")
        && values.get("chat_mode").and_then(Value::as_str) == Some("chat")
        && values.get("pro_mode").and_then(Value::as_bool) == Some(true)
        && matches!(
            values.get("action").and_then(Value::as_str),
            Some("attached" | "already_attached")
        )
        && !values.contains_key("failed_stage");
    let failed_stage = values.get("failed_stage").and_then(Value::as_str);
    let failed = values.get("status").and_then(Value::as_str) == Some("failed")
        && values.get("chat_mode").and_then(Value::as_str) == Some("unverified")
        && values.get("pro_mode").and_then(Value::as_bool) == Some(false)
        && values.get("action").and_then(Value::as_str) == Some("none")
        && failed_stage.is_some_and(|stage| !stage.is_empty());
    verified || failed
}
