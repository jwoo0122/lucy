use serde_json::{json, Value};

use crate::context_budget::{usable_context, COMPACTION_OUTPUT_RESERVE_TOKENS};
use crate::model::{estimate_context_tokens, ChatMessage};

const HISTORY_OPEN: &str = "<discarded_history_json>\n";
const HISTORY_CLOSE: &str = "\n</discarded_history_json>";

pub(crate) fn summary_attempts(
    planned: Vec<ChatMessage>,
    context_window: Option<usize>,
) -> Result<Vec<Vec<ChatMessage>>, String> {
    let payload = planned
        .last()
        .and_then(|message| message.content.as_deref())
        .ok_or_else(|| "compaction plan has no transcript payload".to_owned())?;
    let (_, history) = payload
        .split_once(HISTORY_OPEN)
        .ok_or_else(|| "compaction plan has no discarded history".to_owned())?;
    let (history, _) = history
        .split_once(HISTORY_CLOSE)
        .ok_or_else(|| "compaction plan has an unterminated discarded history".to_owned())?;
    let records: Vec<Value> = serde_json::from_str(history)
        .map_err(|error| format!("invalid discarded history in compaction plan: {error}"))?;
    if records.is_empty() {
        return Err("compaction plan has empty discarded history".to_owned());
    }

    let mut candidates = vec![planned.clone()];
    candidates.push(rewrite_attempt(&planned, &records, records.len(), 500)?);
    candidates.push(rewrite_attempt(
        &planned,
        &records,
        records.len().div_ceil(2),
        500,
    )?);
    candidates.push(rewrite_attempt(
        &planned,
        &records,
        records.len().div_ceil(4),
        250,
    )?);
    candidates.dedup_by(|left, right| left == right);

    if let Some(window) = context_window {
        let usable = usable_context(window, COMPACTION_OUTPUT_RESERVE_TOKENS);
        let mut fitting = candidates
            .iter()
            .filter(|attempt| estimate_context_tokens(attempt) <= usable)
            .cloned()
            .collect::<Vec<_>>();
        if fitting.is_empty() {
            if let Some(smallest) = candidates.last().cloned() {
                fitting.push(smallest);
            }
        }
        return Ok(fitting);
    }
    Ok(candidates)
}

fn rewrite_attempt(
    planned: &[ChatMessage],
    records: &[Value],
    keep_latest: usize,
    max_string_chars: usize,
) -> Result<Vec<ChatMessage>, String> {
    let keep_latest = keep_latest.max(1).min(records.len());
    let omitted = records.len() - keep_latest;
    let mut reduced = records[omitted..].to_vec();
    for record in &mut reduced {
        truncate_json(record, max_string_chars);
    }
    if omitted > 0 {
        reduced.insert(
            0,
            json!({
                "record": "omitted_oldest_history",
                "count": omitted,
                "reason": "compaction input exceeded the reserved context budget"
            }),
        );
    }

    let mut attempt = planned.to_vec();
    let payload = attempt
        .last_mut()
        .and_then(|message| message.content.as_mut())
        .ok_or_else(|| "compaction plan has no mutable transcript payload".to_owned())?;
    let (prefix, history_and_suffix) = payload
        .split_once(HISTORY_OPEN)
        .ok_or_else(|| "compaction plan has no discarded history".to_owned())?;
    let (_, suffix) = history_and_suffix
        .split_once(HISTORY_CLOSE)
        .ok_or_else(|| "compaction plan has an unterminated discarded history".to_owned())?;
    let encoded = serde_json::to_string(&reduced)
        .map_err(|error| format!("unable to encode reduced compaction history: {error}"))?;
    *payload = format!("{prefix}{HISTORY_OPEN}{encoded}{HISTORY_CLOSE}{suffix}");
    Ok(attempt)
}

fn truncate_json(value: &mut Value, max_string_chars: usize) {
    match value {
        Value::String(text) => {
            let mut characters = text.chars();
            let prefix = characters
                .by_ref()
                .take(max_string_chars)
                .collect::<String>();
            if characters.next().is_some() {
                *text = format!(
                    "{prefix}\n[truncated for compaction fallback; field exceeded {max_string_chars} characters]"
                );
            }
        }
        Value::Array(values) => {
            for value in values {
                truncate_json(value, max_string_chars);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                truncate_json(value, max_string_chars);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planned(records: Vec<Value>) -> Vec<ChatMessage> {
        vec![
            ChatMessage::system("boot".to_owned()),
            ChatMessage::system("summarize".to_owned()),
            ChatMessage::user(format!(
                "<previous_summary>\nold\n</previous_summary>\n\n{HISTORY_OPEN}{}{HISTORY_CLOSE}",
                serde_json::to_string(&records).expect("records")
            )),
        ]
    }

    #[test]
    fn progressively_reduces_old_history() {
        let attempts = summary_attempts(
            planned(vec![
                json!({"content": "one"}),
                json!({"content": "two"}),
                json!({"content": "three"}),
                json!({"content": "four"}),
            ]),
            None,
        )
        .expect("attempts");
        assert_eq!(attempts.len(), 3);
        let smallest = attempts.last().expect("smallest")[2]
            .content
            .as_deref()
            .expect("payload");
        assert!(smallest.contains("omitted_oldest_history"));
        assert!(smallest.contains("four"));
        assert!(!smallest.contains("one"));
    }

    #[test]
    fn uses_the_smallest_attempt_when_none_fit_preflight() {
        let attempts = summary_attempts(
            planned(vec![json!({"content": "x".repeat(8_000)})]),
            Some(1_000),
        )
        .expect("attempts");
        assert_eq!(attempts.len(), 1);
        let payload = attempts[0][2].content.as_deref().expect("payload");
        assert!(payload.contains("truncated for compaction fallback"));
    }
}
