use serde_json::{json, Map, Value};

use crate::model::{estimate_message_tokens, ChatMessage};

pub(crate) const PROTECTED_TOOL_OUTPUT_TOKENS: usize = 12_000;
const PRUNED_MARKER: &str = "lucy.tool_output_pruned.v1";
const MAX_RETAINED_FIELD_CHARS: usize = 512;

pub(crate) fn prune_old_tool_outputs(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut pruned = messages.to_vec();
    let mut protected_tokens = 0usize;
    for message in pruned.iter_mut().rev() {
        if message.role != "tool" || is_non_prunable(message) || is_pruned(message) {
            continue;
        }
        let tokens = estimate_message_tokens(message);
        if protected_tokens.saturating_add(tokens) <= PROTECTED_TOOL_OUTPUT_TOKENS {
            protected_tokens = protected_tokens.saturating_add(tokens);
            continue;
        }
        replace_with_placeholder(message);
    }
    pruned
}

fn is_non_prunable(message: &ChatMessage) -> bool {
    matches!(
        message.name.as_deref(),
        Some("skill" | "read_skill" | "load_skill" | "instruction")
    )
}

fn is_pruned(message: &ChatMessage) -> bool {
    message
        .content
        .as_deref()
        .and_then(|content| serde_json::from_str::<Value>(content).ok())
        .and_then(|value| value.get("marker").and_then(Value::as_str).map(str::to_owned))
        .as_deref()
        == Some(PRUNED_MARKER)
}

fn replace_with_placeholder(message: &mut ChatMessage) {
    let original = message.content.take().unwrap_or_default();
    let parsed = serde_json::from_str::<Value>(&original).ok();
    let concise = parsed.as_ref().map(extract_concise_fields);
    let status = parsed
        .as_ref()
        .and_then(infer_status)
        .unwrap_or("completed");
    let placeholder = json!({
        "marker": PRUNED_MARKER,
        "tool_name": message.name.as_deref().unwrap_or("unknown"),
        "call_id": message.tool_call_id.as_deref().unwrap_or("unknown"),
        "status": status,
        "original_bytes": original.len(),
        "pruned": true,
        "rerun": false,
        "retained": concise,
    });
    message.content = serde_json::to_string(&placeholder).ok();
    message.reasoning_details = None;
}

fn infer_status(value: &Value) -> Option<&'static str> {
    if value
        .get("canceled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || value.get("status").and_then(Value::as_str) == Some("canceled")
    {
        return Some("canceled");
    }
    if value
        .get("timed_out")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || value.get("status").and_then(Value::as_str) == Some("timeout")
    {
        return Some("timeout");
    }
    if value
        .get("exit_code")
        .and_then(Value::as_i64)
        .is_some_and(|code| code != 0)
        || value.get("status").and_then(Value::as_str) == Some("failed")
    {
        return Some("failed");
    }
    value
        .get("status")
        .and_then(Value::as_str)
        .map(|status| match status {
            "running" => "running",
            "completed" | "success" => "completed",
            _ => "completed",
        })
}

fn extract_concise_fields(value: &Value) -> Value {
    const KEYS: [&str; 11] = [
        "exit_code",
        "status",
        "timed_out",
        "canceled",
        "path",
        "file",
        "files",
        "cwd",
        "command",
        "background_id",
        "signal",
    ];
    let mut retained = Map::new();
    for key in KEYS {
        if let Some(field) = find_field(value, key) {
            retained.insert(key.to_owned(), bound_value(field));
        }
    }
    Value::Object(retained)
}

fn find_field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|value| find_field(value, key))),
        Value::Array(values) => values.iter().find_map(|value| find_field(value, key)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
    }
}

fn bound_value(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(bound_text(text)),
        Value::Array(values) => Value::Array(values.iter().take(16).map(bound_value).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .take(16)
                .map(|(key, value)| (key.clone(), bound_value(value)))
                .collect(),
        ),
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
    }
}

fn bound_text(text: &str) -> String {
    let mut characters = text.chars();
    let prefix = characters
        .by_ref()
        .take(MAX_RETAINED_FIELD_CHARS)
        .collect::<String>();
    if characters.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(id: &str, content: String) -> ChatMessage {
        ChatMessage::tool(id.to_owned(), "cmd".to_owned(), content)
    }

    #[test]
    fn protects_recent_outputs_and_prunes_older_oversized_results() {
        let messages = vec![
            tool(
                "old",
                format!("{{\"exit_code\":0,\"path\":\"src/lib.rs\",\"stdout\":\"{}\"}}", "x".repeat(60_000)),
            ),
            tool("recent", "{\"exit_code\":0,\"stdout\":\"ok\"}".to_owned()),
        ];
        let pruned = prune_old_tool_outputs(&messages);
        assert!(pruned[0].content.as_deref().unwrap_or_default().contains(PRUNED_MARKER));
        assert!(pruned[0].content.as_deref().unwrap_or_default().contains("src/lib.rs"));
        assert_eq!(pruned[1], messages[1]);
        assert_eq!(messages[0].tool_call_id.as_deref(), Some("old"));
    }

    #[test]
    fn an_oversized_latest_result_is_replaced_to_fit_the_protected_budget() {
        let messages = vec![tool("latest", "x".repeat(100_000))];
        let pruned = prune_old_tool_outputs(&messages);
        let content = pruned[0].content.as_deref().expect("placeholder");
        assert!(content.contains(PRUNED_MARKER));
        assert!(content.len() < 1_000);
    }

    #[test]
    fn pruning_is_deterministic_idempotent_and_does_not_mutate_raw_history() {
        let messages = vec![tool("old", "x".repeat(100_000))];
        let first = prune_old_tool_outputs(&messages);
        let second = prune_old_tool_outputs(&first);
        assert_eq!(first, second);
        assert_eq!(messages[0].content.as_deref(), Some("x".repeat(100_000).as_str()));
    }

    #[test]
    fn canceled_and_failed_results_remain_distinguishable() {
        let canceled = prune_old_tool_outputs(&[tool(
            "canceled",
            format!("{{\"canceled\":true,\"stdout\":\"{}\"}}", "x".repeat(100_000)),
        )]);
        let failed = prune_old_tool_outputs(&[tool(
            "failed",
            format!("{{\"exit_code\":2,\"stdout\":\"{}\"}}", "x".repeat(100_000)),
        )]);
        assert!(canceled[0].content.as_deref().unwrap_or_default().contains("canceled"));
        assert!(failed[0].content.as_deref().unwrap_or_default().contains("failed"));
    }
}
