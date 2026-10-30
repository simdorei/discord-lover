//! Offline-only stdio server for exercising the production Reserve controller.
//! No credentials, network, real Codex process, or user conversation is used.
use super::{
    Logging, Path, Result, Value, emit, env, json, method, reply, serve, thread, wait_file,
};
use std::{collections::BTreeMap, path::PathBuf};

pub(super) fn run() -> Result {
    let log = env("RESERVE_TEST_LOG");
    let root = Path::new(&log).parent().ok_or("fixture root missing")?;
    // Opt-in persistence applies only to settings/history, never quota observations.
    let saved_path = std::env::var_os("RESERVE_TEST_STATE").map(PathBuf::from);
    let saved: Value = match saved_path.as_deref().filter(|path| path.exists()) {
        Some(path) => serde_json::from_slice(&std::fs::read(path)?)?,
        None => json!({"settings":{},"turns":{},"sequence":0}),
    };
    let mut fixture = Fixture {
        log: PathBuf::from(&log),
        root: root.to_path_buf(),
        saved_path,
        initial: json!({"model":"model-a","effort":"high","serviceTier":"priority"}),
        settings: serde_json::from_value(saved["settings"].clone())?,
        turns: serde_json::from_value(saved["turns"].clone())?,
        options: json!({"ordinary":false,"account":"account-a","used":1}),
        sequence: saved["sequence"]
            .as_u64()
            .ok_or("fixture sequence missing")?,
    };
    serve(Path::new(&log), Logging::All, |request| {
        fixture.handle(&request)
    })
}

struct Fixture {
    log: PathBuf,
    root: PathBuf,
    saved_path: Option<PathBuf>,
    initial: Value,
    settings: BTreeMap<String, Value>,
    turns: BTreeMap<String, Vec<Value>>,
    options: Value,
    sequence: u64,
}

impl Fixture {
    fn handle(&mut self, request: &Value) -> Result {
        let target = thread(request).to_owned();
        let operation = method(request);
        if self.options["gate"].as_str() == Some(operation) {
            self.options["gate"] = Value::Null;
            std::fs::write(self.root.join("gate-ready"), operation.as_bytes())?;
            wait_file(&self.root.join("gate-release"), 15, true)?;
        }
        match operation {
            "initialize" => reply(request, &json!({"userAgent":"reserve-native-fixture/3"})),
            "test/question" => {
                self.turns.entry(target.clone()).or_insert_with(|| {
                    vec![json!({"id":"original","status":"completed","items":[]})]
                });
                super::async_question::emit_question("original")?;
                reply(request, &json!({}))
            }
            "test/active-turn" => {
                self.turns.insert(
                    target.clone(),
                    vec![json!({"id":"original","status":"inProgress","items":[]})],
                );
                super::turn(&target, "original", false)?;
                reply(request, &json!({}))
            }
            "thread/turns/list" => {
                let rows = self.turns.get(&target).cloned().unwrap_or_default();
                reply(
                    request,
                    &json!({"data":rows.into_iter().rev().take(usize::try_from(request["params"]["limit"].as_u64().unwrap_or(1)).unwrap()).collect::<Vec<_>>(),"nextCursor":null}),
                )
            }
            "turn/steer" => {
                assert_eq!(request["params"]["expectedTurnId"], "original");
                assert!(self.turns.get(&target).is_some_and(|rows| {
                    rows.iter()
                        .any(|v| v["id"] == "original" && v["status"] == "inProgress")
                }));
                reply(request, &json!({"turnId":"original"}))
            }
            "test/configure" => self.configure(request, &target),
            "test/complete" => self.complete(request, &target),
            "account/rateLimits/read" => self.rate_limits(request),
            "model/list" => reply(request, &effort_catalog(&self.options)),
            "thread/resume" | "thread/read" => self.read_thread(request, &target),
            "thread/settings/update" => self.update_settings(request, &target),
            "turn/start" | "test/goal-turn" => self.start_turn(request, &target, operation),
            "thread/goal/get" => {
                let goal = self.options["goal"].as_str().map_or(
                    Value::Null,
                    |status| json!({"threadId":target,"status":status}),
                );
                reply(request, &json!({"goal":goal}))
            }
            _ => super::error(request, -32601, "unexpected reserve fixture method"),
        }
    }

    fn configure(&mut self, request: &Value, target: &str) -> Result {
        for (key, value) in request["params"].as_object().ok_or("options missing")? {
            if key == "settings" {
                self.settings.insert(target.to_owned(), value.clone());
            } else {
                self.options[key] = value.clone();
            }
        }
        self.persist()?;
        reply(request, &json!({}))
    }

    fn complete(&mut self, request: &Value, target: &str) -> Result {
        let id = request["params"]["turnId"].as_str().ok_or("turn missing")?;
        let status = request["params"]["status"].as_str().unwrap_or("completed");
        let error = if status == "failed" {
            self.options["ordinary"] = json!(false);
            json!({"message":"fixture usage exhausted","codexErrorInfo":"usageLimitExceeded"})
        } else {
            Value::Null
        };
        let items = request["params"]["text"].as_str().map_or_else(
            || json!([]),
            |text| json!([{"type":"agentMessage","text":text,"phase":"final_answer"}]),
        );
        let turn = json!({"id":id,"status":status,"error":error,"items":items});
        if let Some(rows) = self.turns.get_mut(target) {
            for old in rows {
                if old["id"] == id {
                    *old = turn.clone();
                }
            }
        }
        self.persist()?;
        if self.options["emit_final_answer"].as_bool() == Some(true)
            && let Some(text) = request["params"]["text"].as_str()
        {
            emit(&json!({"method":"item/completed","params":{
                "threadId":target,"turnId":id,"item":{
                    "id":format!("final-{id}"),"type":"agentMessage",
                    "phase":"final_answer","text":text
                }
            }}))?;
        }
        emit(&json!({"method":"turn/completed","params":{"threadId":target,"turn":turn}}))?;
        reply(request, &json!({"threadId":target,"turn":turn}))
    }

    fn rate_limits(&mut self, request: &Value) -> Result {
        let options = &mut self.options;
        let mut result = json!({"accountId":options["account"],"ordinaryUsageAllowed":options["ordinary"],
        "rateLimits":{"primary":{"usedPercent":100}},
        "rateLimitsByLimitId":{"base_model_inference":{
            "limitId":"base_model_inference","limitName":"gpt-reserve","normalModelSlug":options.get("normal").cloned().unwrap_or(json!("gpt-5.6-luna")),
            "primary":{"usedPercent":options["used"],"windowDurationMins":10080},
            "secondary":null,"rateLimitReachedType":options["reached"],"spendControlReached":options.get("spend").cloned().unwrap_or(json!(false))
        }}});
        if options["omit_ordinary"].as_bool() == Some(true) {
            result
                .as_object_mut()
                .unwrap()
                .remove("ordinaryUsageAllowed");
        }
        // Construct this response before the one-shot change to the next read.
        if let Some(after) = options["after_rates"].as_object().cloned() {
            options["after_rates"] = Value::Null;
            for (key, value) in after {
                options[key] = value;
            }
        }
        reply(request, &result)
    }

    fn read_thread(&mut self, request: &Value, target: &str) -> Result {
        let state = self
            .settings
            .entry(target.to_owned())
            .or_insert_with(|| self.initial.clone());
        let rows = self.turns.entry(target.to_owned()).or_default();
        let active = rows.iter().any(|v| v["status"] == "inProgress");
        let id = if self.options["wrong_thread"].as_bool() == Some(true) {
            "foreign-thread"
        } else {
            target
        };
        let visible: Vec<_> = rows
            .iter()
            .filter(|row| {
                self.options["omit_turn"]
                    .as_str()
                    .is_none_or(|omit| row["id"].as_str() != Some(omit))
            })
            .collect();
        let mut value = json!({"thread":{"id":id,"status":{"type":if active {"active"} else {"idle"}},"turns":visible},
            "model":state["model"],"reasoningEffort":state["effort"],"serviceTier":state["serviceTier"]});
        if let Some(turn) = self.options["history_items_turn"].as_str()
            && let Some(items) = self.options.get("history_items")
        {
            for row in value["thread"]["turns"].as_array_mut().unwrap() {
                if row["id"].as_str() == Some(turn) {
                    row["items"] = items.clone();
                }
            }
        }
        if self.options["missing_effort"].as_bool() == Some(true) {
            value.as_object_mut().unwrap().remove("reasoningEffort");
        }
        reply(request, &value)
    }

    fn update_settings(&mut self, request: &Value, target: &str) -> Result {
        if self
            .turns
            .get(target)
            .is_some_and(|rows| rows.iter().any(|v| v["status"] == "inProgress"))
        {
            return super::error(
                request,
                -32600,
                "fixture prohibits settings mutation during active work",
            );
        }
        let state = self
            .settings
            .entry(target.to_owned())
            .or_insert_with(|| self.initial.clone());
        let previous = state.clone();
        for field in ["model", "effort", "serviceTier"] {
            if let Some(value) = request["params"].get(field) {
                state[field] = value.clone();
            }
        }
        if let Some(effort) = self.options.get("applied_effort_override") {
            state["effort"] = effort.clone();
        }
        // Change only the next account response, after settings really apply.
        if let Some(after) = self.options["after_settings"].as_object().cloned() {
            self.options["after_settings"] = Value::Null;
            for (key, value) in after {
                self.options[key] = value;
            }
        }
        if self.options["suppress_observation"].as_bool() != Some(true) && *state != previous {
            emit(
                &json!({"method":"thread/settings/updated","params":{"threadId":target,"threadSettings":state}}),
            )?;
        }
        self.persist()?;
        reply(request, &json!({}))
    }

    fn start_turn(&mut self, request: &Value, target: &str, operation: &str) -> Result {
        if operation == "test/goal-turn"
            && (self.options["goal"].as_str() != Some("active")
                || self
                    .turns
                    .get(target)
                    .is_some_and(|rows| rows.iter().any(|v| v["status"] == "inProgress")))
        {
            return super::error(
                request,
                -32600,
                "automatic goal turn requires an idle active goal",
            );
        }
        if let Some(rejection) = self.options["reject_next_start"]
            .as_str()
            .map(str::to_owned)
        {
            self.options["reject_next_start"] = Value::Null;
            if rejection == "usage" {
                self.options["ordinary"] = json!(false);
            }
            return emit(
                &json!({"id":request["id"],"error":{"code":-32000,"message":"fixture start rejection",
                "data":{"codexErrorInfo":if rejection == "usage" {"usageLimitExceeded"} else {"rateLimitExceeded"}}}}),
            );
        }
        self.sequence += 1;
        let id = format!("fixture-turn-{}", self.sequence);
        let state = self
            .settings
            .entry(target.to_owned())
            .or_insert_with(|| self.initial.clone());
        super::append(
            &self.log,
            &json!({"event":"start_settings","threadId":target,"turnId":id,"settings":state}),
        )?;
        let turn = json!({"id":id,"status":"inProgress","items":[],"error":null});
        self.turns
            .entry(target.to_owned())
            .or_default()
            .push(turn.clone());
        self.persist()?;
        emit(&json!({"method":"turn/started","params":{"threadId":target,"turn":turn}}))?;
        reply(request, &json!({"turn":turn}))
    }

    fn persist(&self) -> Result {
        if let Some(path) = self.saved_path.as_deref() {
            std::fs::write(
                path,
                serde_json::to_vec(&json!({
                    "settings":self.settings,"turns":self.turns,"sequence":self.sequence
                }))?,
            )?;
        }
        Ok(())
    }
}

// Defaults retain the old fixture contract; each override is test/configure-only.
fn effort_catalog(options: &Value) -> Value {
    let mut result = json!({"data":[
        {"model":"model-a","defaultReasoningEffort":"high","supportedReasoningEfforts":[{"reasoningEffort":"high"},{"reasoningEffort":"low"}]},
        {"model":"model-b","defaultReasoningEffort":"medium","supportedReasoningEfforts":[{"reasoningEffort":"medium"}]},
        {"model":"gpt-5.6-luna","defaultReasoningEffort":"medium","supportedReasoningEfforts":[{"reasoningEffort":"medium"},{"reasoningEffort":"high"},{"reasoningEffort":"xhigh"}]}
    ]});
    if let Some(default) = options.get("reserve_default") {
        if default.is_null() {
            result["data"][2]
                .as_object_mut()
                .unwrap()
                .remove("defaultReasoningEffort");
        } else {
            result["data"][2]["defaultReasoningEffort"] = default.clone();
        }
    }
    if let Some(supported) = options.get("reserve_efforts") {
        result["data"][2]["supportedReasoningEfforts"] = supported.clone();
    }
    result
}
