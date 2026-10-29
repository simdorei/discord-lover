use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}
fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(root().join(path)).unwrap()
}

#[test]
fn packaged_probe_fingerprints_match_native_verifiers() {
    use cdr_pro::evidence::{BROWSER_PROBE_SHA256, CONNECTOR_PROBE_SHA256};

    for (file, expected) in [
        ("browser_evidence_probe.mjs", BROWSER_PROBE_SHA256),
        ("pro_connector_control.mjs", CONNECTOR_PROBE_SHA256),
    ] {
        let source = read(format!(
            "plugins/codex-discord-remote/skills/ask-chatgpt-pro/scripts/{file}"
        ));
        let canonical = source.replace("\r\n", "\n").replace('\r', "\n");
        assert_eq!(
            hex::encode(Sha256::digest(canonical.as_bytes())),
            expected,
            "{file}: reviewed script and native verifier fingerprint must change together"
        );
    }
}

#[test]
fn packaged_skills_and_browser_scripts_match_the_project_sources() {
    let base = "plugins/codex-discord-remote/skills/ask-chatgpt-pro";
    for file in [
        "SKILL.md",
        "scripts/browser_evidence_probe.mjs",
        "scripts/pro_connector_control.mjs",
        "agents/openai.yaml",
    ] {
        assert_eq!(
            read(format!("{base}/{file}")),
            read(format!(".agents/skills/ask-chatgpt-pro/{file}")),
            "{file}"
        );
    }
    let plugin = root().join("plugins/codex-discord-remote");
    let manifest: Value = serde_json::from_str(&read(
        "plugins/codex-discord-remote/.codex-plugin/plugin.json",
    ))
    .unwrap();
    assert_eq!(manifest["skills"], "./skills/");
    assert!(manifest["interface"]["longDescription"].is_string());
    assert!(
        regex::Regex::new(r"^0\.1\.0\+codex\.[0-9]{14}$")
            .unwrap()
            .is_match(manifest["version"].as_str().unwrap())
    );
    assert!(manifest.get("hooks").is_none());
    for file in [
        "skills/deep-interview/auto-research-greenfield.md",
        "skills/deep-interview/auto-answer-uncertain.md",
        "skills/intent-driven-qa/SKILL.md",
        "skills/intent-driven-qa/agents/openai.yaml",
    ] {
        assert!(plugin.join(file).is_file());
    }
    assert!(
        read("plugins/codex-discord-remote/skills/deep-interview/NOTICE.md")
            .contains("MIT License")
    );
    assert!(
        read("plugins/codex-discord-remote/skills/deep-interview/SKILL.md")
            .contains("name: deep-interview")
    );
    assert!(
        read("plugins/codex-discord-remote/skills/intent-driven-qa/SKILL.md")
            .contains("name: intent-driven-qa")
    );
    assert!(
        read("plugins/codex-discord-remote/skills/intent-driven-qa/agents/openai.yaml")
            .contains("$intent-driven-qa")
    );
    assert!(read("docs/plugin-skills.md").contains("$codex-discord-remote:intent-driven-qa"));
    for removed in ["github-project-triage", "maintainer-orchestrator"] {
        assert!(!plugin.join("skills").join(removed).exists());
    }
}
#[test]
fn pro_consultation_ownership_and_no_replay_guards_are_retained() {
    let skill = read("plugins/codex-discord-remote/skills/ask-chatgpt-pro/SKILL.md");
    for required in [
        "select_project",
        "file_apply_patch",
        "git_push",
        "cdr-pro-helper",
        "restart --scope <conversation_scope> --failed-url <failed-canonical-url>",
        "thinking_failure_restart_used",
        "status --scope <conversation_scope>",
        "complete-restart --scope <conversation_scope>",
        "restore-stalled --scope <conversation_scope>",
        "never poll with `acquire`",
        "Never run `restore-stalled` automatically",
        "`status: protected`",
        "must not equal the failed canonical conversation",
        "Expiry marks the owner as stalled; it does not revoke",
        "persists across Codex process restarts",
        "immutable copy of the exact initial consultation request",
        "Never delete or archive the failed ChatGPT conversation",
        "A spinner or active Stop control, elapsed time, partial output",
        "never accept that partial text as the completed answer",
        "Do not replay a request with possible external side effects",
        "[@Chrome](plugin://chrome@openai-bundled)",
        "browser-probe-code",
        "connector-probe-code",
        "connector-retry-probe-code",
    ] {
        assert!(
            skill.contains(required),
            "missing unchanged rule: {required}"
        );
    }
    let normalized = skill.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(normalized.contains("Keep a turn-local `connector_retry_used` flag initialized to false. Immediately before every retry helper invocation, including fresh-chat recovery below, set it to true; if it is already true, stop without sending."));
    let start = normalized
        .find("For a contenteditable ChatGPT composer")
        .unwrap();
    let end = normalized
        .find("After selection, prefer the dedicated file tools")
        .unwrap();
    assert_eq!(
        normalized[start..end].trim(),
        include_str!("../../../fixtures/parity/pro_composer_instruction.txt").trim()
    );
}
#[test]
fn hook_manifests_call_native_helpers_on_both_platforms() {
    for (file, command) in [
        ("browser-evidence.json", "browser-hook"),
        ("pro-connector-evidence.json", "connector-hook"),
    ] {
        let value: Value =
            serde_json::from_str(&read(format!("plugins/codex-discord-remote/hooks/{file}")))
                .unwrap();
        for entries in value["hooks"].as_object().unwrap().values() {
            for entry in entries.as_array().unwrap() {
                for hook in entry["hooks"].as_array().unwrap() {
                    assert_eq!(hook["type"], "command");
                    assert_eq!(
                        hook["command"],
                        format!("\"${{PLUGIN_ROOT}}/bin/cdr-pro-helper\" {command}")
                    );
                    assert_eq!(
                        hook["commandWindows"],
                        format!("\"${{PLUGIN_ROOT}}\\bin\\cdr-pro-helper.exe\" {command}")
                    );
                }
            }
        }
    }
    assert!(root().join("crates/cdr-pro/src/bin/helper.rs").is_file());
}
