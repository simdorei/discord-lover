use super::{
    Duration, Instant, Logging, Path, Result, Value, emit, env, error, json, method, reply, serve,
    thread, wait_file,
};

fn observe(thread: &str, settings: &Value) -> Result {
    emit(
        &json!({"method":"thread/settings/updated","params":{"threadId":thread,"threadSettings":settings}}),
    )
}

fn quota(mode: &str) -> Value {
    if !mode.starts_with("reserve") {
        return json!({"rateLimitsByLimitId":null});
    }
    let mut rates = json!({"ordinaryUsageAllowed":false,"rateLimitsByLimitId":{
        "base_model_inference":{"limitId":"base_model_inference","limitName":"gpt-reserve","normalModelSlug":"gpt-5.6-luna","primary":{"usedPercent":12,"windowDurationMins":10_080,"resetsAt":2_000_000_000},"secondary":null,"rateLimitReachedType":null,"spendControlReached":false}
    }});
    if mode == "reserve-exhausted" {
        rates["rateLimitsByLimitId"]["base_model_inference"]["primary"]["usedPercent"] = json!(100);
    }
    if mode == "reserve-metadata-missing" {
        rates["rateLimitsByLimitId"]["base_model_inference"]["normalModelSlug"] = Value::Null;
    }
    rates
}

fn update_effective_settings(mode: &str, request: &Value, settings: &mut Value) {
    for key in ["model", "effort", "serviceTier"] {
        if let Some(value) = request["params"].get(key) {
            // Reproduce the installed server's effective default-tier observation.
            settings[key] = if key == "serviceTier"
                && matches!(
                    mode,
                    "reserve-default-tier" | "standard-noop" | "standard-clear"
                )
                && value.is_null()
            {
                json!("default")
            } else {
                value.clone()
            };
        }
    }
    match mode {
        "reserve-tier-mismatch" => settings["serviceTier"] = json!("priority"),
        "reserve-model-mismatch" => settings["model"] = json!("gpt-5.6-luna"),
        "reserve-effort-mismatch" => settings["effort"] = json!("high"),
        "mismatch" => settings["model"] = json!("model-a"),
        _ => {}
    }
}

fn initial_settings(mode: &str) -> Value {
    let mut settings = json!({"model":"model-a","effort":"high","serviceTier":null});
    if mode == "reserve-active" {
        settings["model"] = json!("gpt-reserve");
    }
    if matches!(mode, "reserve-default-tier" | "standard-noop") {
        settings["serviceTier"] = json!("default");
    }
    if mode == "standard-clear" {
        settings["serviceTier"] = json!("priority");
    }
    if mode.starts_with("reserve-noop") {
        settings = json!({"model":"gpt-reserve","effort":"xhigh","serviceTier":"default"});
    }
    settings
}

pub(super) fn run() -> Result {
    let mode = env("SETTINGS_TEST_MODE");
    let log = env("SETTINGS_TEST_LOG");
    let root = Path::new(&log).parent().unwrap();
    let mut settings = initial_settings(&mode);
    serve(Path::new(&log), Logging::All, |request| {
        let thread = thread(&request);
        let result = match method(&request) {
            "initialize" => json!({"userAgent":"settings-test/1"}),
            "test/observe" => {
                observe(thread, &settings)?;
                json!({})
            }
            "test/pauseReads" | "test/blocked" => json!({}),
            "account/rateLimits/read" => quota(&mode),
            "model/list" => {
                let mut catalog = json!({"data":[
                    {"model":"model-a","displayName":"Model A","supportedReasoningEfforts":[{"reasoningEffort":"high"},{"reasoningEffort":"low"}]},
                    {"model":"model-b","displayName":"Model B","supportedReasoningEfforts":[{"reasoningEffort":"medium"}]}]});
                if mode.starts_with("reserve") {
                    catalog["data"].as_array_mut().unwrap().push(json!({"model":"gpt-5.6-luna","displayName":"GPT-5.6-Luna","defaultReasoningEffort":"medium","supportedReasoningEfforts":[{"reasoningEffort":"medium"},{"reasoningEffort":"high"},{"reasoningEffort":"xhigh"}]}));
                }
                catalog
            }
            "thread/read" => json!({"thread":{"id":thread,"status":{"type":"idle"},"turns":[]}}),
            "thread/resume" => {
                let mut result = json!({"thread":{"id":thread,"status":{"type":"idle"},"turns":[]},"model":settings["model"],"reasoningEffort":settings["effort"],"serviceTier":settings["serviceTier"]});
                if matches!(mode.as_str(), "wrong-thread" | "reserve-noop-wrong-thread") {
                    result["thread"]["id"] = json!("other-thread");
                }
                if matches!(
                    mode.as_str(),
                    "incomplete-resume" | "reserve-noop-incomplete"
                ) {
                    result.as_object_mut().unwrap().remove("serviceTier");
                }
                if mode == "watermark" {
                    std::fs::write(root.join("resume-ready"), "ready")?;
                    wait_file(&root.join("resume-release"), 40, true)?;
                }
                result
            }
            "thread/settings/update" => {
                if matches!(mode.as_str(), "reject" | "reserve-reject") {
                    return error(&request, -32600, "fixture settings rejection");
                }
                let previous = settings.clone();
                update_effective_settings(&mode, &request, &mut settings);
                let unchanged = (mode.starts_with("reserve-noop") || mode == "standard-noop")
                    && settings == previous;
                if !unchanged
                    && !matches!(
                        mode.as_str(),
                        "missing" | "watermark" | "reserve-missing-observation"
                    )
                {
                    observe(thread, &settings)?;
                }
                json!({})
            }
            _ => return error(&request, -32601, "unexpected fixture method"),
        };
        reply(&request, &result)?;
        if method(&request) == "thread/resume" && mode == "watermark" {
            observe(thread, &settings)?;
            let deadline = Instant::now() + Duration::from_secs(40);
            let mut emitted = false;
            while !root.join("writer-release").exists() {
                if !emitted && root.join("premature-emit").exists() {
                    settings["model"] = json!("model-b");
                    observe(thread, &settings)?;
                    emitted = true;
                }
                if Instant::now() >= deadline {
                    return Err("settings writer fixture gate timed out".into());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        if method(&request) == "test/pauseReads" {
            wait_file(
                Path::new(
                    request["params"]["releasePath"]
                        .as_str()
                        .ok_or("missing releasePath")?,
                ),
                40,
                true,
            )?;
        }
        Ok(())
    })
}
