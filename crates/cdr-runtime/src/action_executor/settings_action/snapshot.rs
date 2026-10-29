use super::ActionError;
use cdr_app_server::requests::{ServiceTierUpdate, ThreadSettingsUpdate};
use serde_json::Value;

pub(crate) struct Settings {
    pub(crate) model: String,
    pub(crate) effort: Option<String>,
    pub(crate) tier: Option<String>,
}
impl Settings {
    pub fn parse(value: &Value) -> Result<Self, ActionError> {
        let model = value
            .get("model")
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| invalid("model missing"))?;
        Ok(Self {
            model: model.into(),
            effort: nullable(value, "effort")?,
            tier: nullable(value, "serviceTier")?,
        })
    }
    pub(crate) fn from_resume(value: &Value) -> Result<Self, ActionError> {
        let mut settings = value
            .as_object()
            .cloned()
            .ok_or_else(|| invalid("resume is not an object"))?;
        if let Some(effort) = settings.remove("reasoningEffort") {
            settings.insert("effort".into(), effort);
        } else {
            // A notification-shaped field is not a complete resume snapshot.
            settings.remove("effort");
        }
        Self::parse(&Value::Object(settings))
    }
    pub fn matches(&self, update: &ThreadSettingsUpdate) -> bool {
        update.model.as_ref().is_none_or(|v| v == &self.model)
            && if update.effort_clear {
                self.effort.is_none()
            } else {
                update
                    .effort
                    .as_ref()
                    .is_none_or(|v| Some(v) == self.effort.as_ref())
            }
            && match &update.service_tier {
                ServiceTierUpdate::Unchanged => true,
                ServiceTierUpdate::Clear => {
                    matches!(self.tier.as_deref(), None | Some("default"))
                }
                ServiceTierUpdate::Set(tier) => Some(tier) == self.tier.as_ref(),
            }
    }
    pub fn display(&self, thread: &str, label: &str) -> String {
        let speed = match self.tier.as_deref() {
            None | Some("default") => "standard",
            Some("priority") => "fast",
            Some(other) => other,
        };
        format!(
            "{label}\nthread: {thread}\n모델: {}\n추론: {}\n속도: {speed}",
            self.model,
            self.effort.as_deref().unwrap_or("모델 기본값")
        )
    }
}
fn nullable(value: &Value, key: &str) -> Result<Option<String>, ActionError> {
    match value.get(key) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text.clone())),
        _ => Err(invalid("setting field missing or malformed")),
    }
}
fn invalid(reason: &str) -> ActionError {
    ActionError::Invalid(format!(
        "settings observation invalid: {reason}; no verified success"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn standard_matches_only_explicit_default_or_null_tier() {
        let update = ThreadSettingsUpdate {
            service_tier: ServiceTierUpdate::Clear,
            ..ThreadSettingsUpdate::default()
        };
        for (tier, expected) in [
            (Value::Null, true),
            (json!("default"), true),
            (json!("priority"), false),
            (json!("unknown"), false),
        ] {
            let settings =
                Settings::parse(&json!({"model":"model-a","effort":"high","serviceTier":tier}))
                    .unwrap();
            assert_eq!(settings.matches(&update), expected);
        }
        assert!(Settings::parse(&json!({"model":"model-a","effort":"high"})).is_err());
    }

    #[test]
    fn incomplete_resume_is_not_invented_as_default_effort_or_standard_speed() {
        for value in [
            json!({"model":"model-a"}),
            json!({"model":"model-a","reasoningEffort":null}),
            json!({"model":"model-a","serviceTier":null}),
        ] {
            assert!(
                Settings::from_resume(&value).is_err(),
                "missing settings are not explicit null: {value}"
            );
        }
        let value = json!({"model":"model-a","reasoningEffort":null,"serviceTier":null});
        assert!(Settings::from_resume(&value).is_ok());
    }
    #[test]
    fn explicit_standard_preserves_exact_verification_and_readable_display() {
        let update = ThreadSettingsUpdate {
            model: Some("gpt-reserve".into()),
            effort: Some("xhigh".into()),
            effort_clear: false,
            service_tier: ServiceTierUpdate::Set("default".into()),
        };
        let observed = json!({"model":"gpt-reserve","effort":"xhigh","serviceTier":"default"});
        let settings = Settings::parse(&observed).unwrap();
        assert!(settings.matches(&update));
        assert!(
            settings
                .display("thread", "verified")
                .contains("속도: standard")
        );
        for (field, different) in [
            ("model", json!("gpt-5.6-luna")),
            ("effort", json!("high")),
            ("serviceTier", json!("priority")),
            ("serviceTier", Value::Null),
        ] {
            let mut value = observed.clone();
            value[field] = different;
            assert!(!Settings::parse(&value).unwrap().matches(&update));
        }
    }
}
