use serde::Serialize;
use serde_json::Value;
use std::{
    env,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Serialize)]
struct Event<'a> {
    version: u32,
    sequence: u64,
    session_id: &'a str,
    wall_time: u64,
    event: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}
pub(crate) trait RunOutput: Send + Sync {
    fn starting(&self, _simulated: bool) {}
    fn emit(
        &self,
        seq: &mut u64,
        sid: &str,
        event: &str,
        data: Option<Value>,
    ) -> Result<(), String>;
}

pub(crate) struct NdjsonOutput {
    pub(crate) quiet: bool,
}

impl RunOutput for NdjsonOutput {
    fn starting(&self, simulated: bool) {
        if !self.quiet {
            eprintln!(
                "starting {} run",
                if simulated { "simulated" } else { "provider" }
            );
        }
    }

    fn emit(
        &self,
        seq: &mut u64,
        sid: &str,
        event: &str,
        data: Option<Value>,
    ) -> Result<(), String> {
        emit(seq, sid, event, data)
    }
}

fn emit(seq: &mut u64, sid: &str, event: &str, data: Option<Value>) -> Result<(), String> {
    *seq += 1;
    let value = serde_json::to_value(Event {
        version: 1,
        sequence: *seq,
        session_id: sid,
        wall_time: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        event,
        data,
    })
    .map_err(|e| format!("event serialization: {e}"))?;
    let value = redact_value(value)?;
    println!(
        "{}",
        serde_json::to_string(&value).map_err(|e| format!("event serialization: {e}"))?
    );
    Ok(())
}
pub(crate) fn redact(input: &str) -> String {
    let secrets = [
        "OPENAI_API_KEY",
        "OPENROUTER_API_KEY",
        "NVIDIA_API_KEY",
        "SERVOLOOP_API_KEY",
    ]
    .into_iter()
    .filter_map(|name| env::var(name).ok())
    .collect::<Vec<_>>();
    redact_text(input, &secrets)
}
pub(crate) fn redact_value(value: Value) -> Result<Value, String> {
    let secrets = [
        "OPENAI_API_KEY",
        "OPENROUTER_API_KEY",
        "NVIDIA_API_KEY",
        "SERVOLOOP_API_KEY",
    ]
    .into_iter()
    .filter_map(|name| env::var(name).ok())
    .collect::<Vec<_>>();
    redact_value_with_secrets(value, &secrets)
}
fn redact_text(input: &str, secrets: &[String]) -> String {
    let mut secrets = secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .collect::<Vec<_>>();
    // Replacing longer values first prevents a short configured secret from
    // leaving the suffix of a longer one visible.
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets.into_iter().fold(input.to_owned(), |text, secret| {
        text.replace(secret, "[REDACTED]")
    })
}
fn redact_value_with_secrets(value: Value, secrets: &[String]) -> Result<Value, String> {
    match value {
        Value::String(text) => Ok(Value::String(redact_text(&text, secrets))),
        Value::Array(values) => values
            .into_iter()
            .map(|value| redact_value_with_secrets(value, secrets))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(values) => {
            let mut redacted = serde_json::Map::new();
            for (key, value) in values {
                let key = redact_text(&key, secrets);
                if redacted.contains_key(&key) {
                    return Err("redaction produced duplicate object keys".into());
                }
                redacted.insert(key, redact_value_with_secrets(value, secrets)?);
            }
            Ok(Value::Object(redacted))
        }
        value => Ok(value),
    }
}
pub(crate) fn print_value(value: Value) -> Result<(), String> {
    let value = redact_value(value)?;
    println!(
        "{}",
        serde_json::to_string(&value).map_err(|e| format!("output serialization: {e}"))?
    );
    Ok(())
}
pub(crate) fn redacted_session(
    session: &servoloop_core::Session,
) -> Result<servoloop_core::Session, String> {
    let value = serde_json::to_value(session).map_err(|e| format!("session serialization: {e}"))?;
    serde_json::from_value(redact_value(value)?).map_err(|e| format!("redacted session: {e}"))
}
pub(crate) fn safe_config(cfg: &crate::config::Config) -> Value {
    let mut value = serde_json::to_value(cfg).unwrap_or(Value::Null);
    if let Some(url) = value
        .get("base_url")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        // Credentials in an URL are never useful in `config` output.
        if let Some(at) = url.find('@') {
            let scheme = url.find("://").map(|i| i + 3).unwrap_or(0);
            if at >= scheme {
                value["base_url"] =
                    Value::String(format!("{}[REDACTED]{}", &url[..scheme], &url[at..]));
            }
        }
    }
    value
}

#[cfg(test)]
mod redaction_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_nested_values_and_object_keys_without_json_round_trip_leaks() {
        let secrets = vec![
            "quote\"secret".into(),
            "slash\\secret".into(),
            "line\nsecret".into(),
            "秘密".into(),
        ];
        let value = json!({
            "quote\"secret": ["quote\"secret", {"nested": "slash\\secret"}],
            "line": "秘密 and line\nsecret",
        });
        let redacted = redact_value_with_secrets(value, &secrets).unwrap();
        let text = redacted.to_string();
        assert_eq!(
            redacted,
            json!({
                "[REDACTED]": ["[REDACTED]", {"nested": "[REDACTED]"}],
                "line": "[REDACTED] and [REDACTED]",
            })
        );
        assert!(!text.contains("secret"));
        assert!(!text.contains("秘密"));
    }

    #[test]
    fn plain_text_redaction_handles_escaped_secret_characters() {
        let secrets = vec!["quote\"secret".into(), "line\nsecret".into()];
        let output = redact_text("error: quote\"secret / line\nsecret", &secrets);
        assert_eq!(output, "error: [REDACTED] / [REDACTED]");
    }

    #[test]
    fn redaction_fails_closed_on_object_key_collisions() {
        let secrets = vec!["secret".into()];
        let value = json!({"secret": 1, "[REDACTED]": 2});
        assert!(redact_value_with_secrets(value, &secrets).is_err());
    }
}
