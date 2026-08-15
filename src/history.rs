use std::io::Write;
use std::path::Path;

use crate::attention::attention_reset_event;
use crate::attention_lock::AttentionLease;
use crate::journal::{Journal, JournalEvent};

const DEFAULT_RECENT: usize = 20;
const DEFAULT_SEARCH_LIMIT: usize = 50;

pub fn run_cli<W: Write, E: Write>(args: &[String], mut output: W, mut error: E) -> i32 {
    let home = match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => std::path::PathBuf::from(home),
        _ => {
            let _ = writeln!(error, "!: HOME is not set");
            return 1;
        }
    };
    run_cli_at_home(args, &mut output, &mut error, &home)
}

pub fn run_cli_at_home<W: Write, E: Write>(
    args: &[String],
    output: &mut W,
    error: &mut E,
    home: &Path,
) -> i32 {
    let journal = Journal::for_home(home);
    let result = if args.first().is_some_and(|command| command == "reset") {
        AttentionLease::acquire(home).and_then(|_lease| reset(args, &journal))
    } else {
        run_command(args, &journal)
    };
    match result {
        Ok(events) => match write_events(output, &events) {
            Ok(()) => 0,
            Err(message) => {
                let _ = writeln!(error, "!: {message}");
                1
            }
        },
        Err(message) => {
            let _ = writeln!(error, "!: {message}");
            if message == usage() {
                2
            } else {
                1
            }
        }
    }
}

fn run_command(args: &[String], journal: &Journal) -> Result<Vec<JournalEvent>, String> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err(usage());
    };
    match command {
        "recent" => recent(args, journal),
        "show" => show(args, journal),
        "search" => search(args, journal),
        "reset" => reset(args, journal),
        _ => Err(usage()),
    }
}

fn reset(args: &[String], journal: &Journal) -> Result<Vec<JournalEvent>, String> {
    if args.len() != 1 {
        return Err(usage());
    }
    let event = attention_reset_event("cli", "history")?;
    journal.append(&event)?;
    Ok(vec![event])
}

fn recent(args: &[String], journal: &Journal) -> Result<Vec<JournalEvent>, String> {
    if args.len() > 2 {
        return Err(usage());
    }
    let count = args
        .get(1)
        .map(|value| parse_usize(value, "recent count"))
        .transpose()?
        .unwrap_or(DEFAULT_RECENT);
    let events = journal.read_all()?;
    let start = events.len().saturating_sub(count);
    Ok(events[start..].to_vec())
}

fn show(args: &[String], journal: &Journal) -> Result<Vec<JournalEvent>, String> {
    let Some(id) = args.get(1) else {
        return Err(usage());
    };
    let mut around = 0usize;
    let mut index = 2;
    while index < args.len() {
        match args[index].as_str() {
            "--around" => {
                let value = args.get(index + 1).ok_or_else(usage)?;
                around = parse_usize(value, "around count")?;
                index += 2;
            }
            _ => return Err(usage()),
        }
    }

    let events = journal.read_all()?;
    let position = events
        .iter()
        .position(|event| event.id == *id)
        .ok_or_else(|| "journal event not found".to_owned())?;
    let start = position.saturating_sub(around);
    let end = position
        .saturating_add(around)
        .saturating_add(1)
        .min(events.len());
    Ok(events[start..end].to_vec())
}

fn search(args: &[String], journal: &Journal) -> Result<Vec<JournalEvent>, String> {
    let Some(query) = args.get(1).filter(|value| !value.is_empty()) else {
        return Err(usage());
    };
    let query = query.to_lowercase();
    let mut cwd = None;
    let mut since_ms = None;
    let mut limit = DEFAULT_SEARCH_LIMIT;
    let mut index = 2;

    while index < args.len() {
        match args[index].as_str() {
            "--cwd" => {
                cwd = Some(args.get(index + 1).ok_or_else(usage)?.clone());
                index += 2;
            }
            "--since-ms" => {
                since_ms = Some(parse_u64(
                    args.get(index + 1).ok_or_else(usage)?,
                    "since timestamp",
                )?);
                index += 2;
            }
            "--limit" => {
                limit = parse_usize(args.get(index + 1).ok_or_else(usage)?, "search limit")?;
                index += 2;
            }
            _ => return Err(usage()),
        }
    }

    let events = journal.read_all()?;
    let mut matches = events
        .into_iter()
        .filter(|event| since_ms.is_none_or(|since| event.timestamp_ms >= since))
        .filter(|event| {
            cwd.as_ref()
                .is_none_or(|cwd| event.cwd.as_ref() == Some(cwd))
        })
        .filter(|event| lexical_text(event).contains(&query))
        .collect::<Vec<_>>();
    if matches.len() > limit {
        matches.drain(..matches.len() - limit);
    }
    Ok(matches)
}

fn lexical_text(event: &JournalEvent) -> String {
    serde_json::to_string(event)
        .unwrap_or_default()
        .to_lowercase()
}

fn write_events<W: Write>(output: &mut W, events: &[JournalEvent]) -> Result<(), String> {
    for event in events {
        serde_json::to_writer(&mut *output, event)
            .map_err(|_| "unable to encode history output".to_owned())?;
        output
            .write_all(b"\n")
            .map_err(|_| "unable to write history output".to_owned())?;
    }
    output
        .flush()
        .map_err(|_| "unable to write history output".to_owned())
}

fn parse_usize(value: &str, name: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|_| format!("invalid {name}"))
}

fn parse_u64(value: &str, name: &str) -> Result<u64, String> {
    value.parse::<u64>().map_err(|_| format!("invalid {name}"))
}

fn usage() -> String {
    "usage: lucy history recent [count] | show <event-id> [--around count] | search <query> [--cwd path] [--since-ms timestamp] [--limit count] | reset".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::{causal_messages, latest_attention_head};
    use crate::journal::JournalEvent;
    use crate::model::ChatMessage;
    use serde_json::json;

    fn root() -> std::path::PathBuf {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).expect("random root");
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        std::env::temp_dir().join(format!("lucy-history-{suffix}"))
    }

    fn append(journal: &Journal, kind: &str, text: &str, cwd: Option<&str>) -> JournalEvent {
        let mut event = JournalEvent::new(kind, json!({"text": text})).expect("event");
        event.cwd = cwd.map(str::to_owned);
        journal.append(&event).expect("append");
        event
    }

    #[test]
    fn recent_returns_tail_in_chronological_order() {
        let root = root();
        let journal = Journal::at_root(root.clone());
        append(&journal, "user_message", "one", None);
        let second = append(&journal, "assistant_message", "two", None);
        let third = append(&journal, "user_message", "three", None);

        let result = recent(&["recent".to_owned(), "2".to_owned()], &journal).expect("recent");
        assert_eq!(result, vec![second, third]);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn show_around_uses_exact_event_identity() {
        let root = root();
        let journal = Journal::at_root(root.clone());
        let first = append(&journal, "user_message", "one", None);
        let second = append(&journal, "assistant_message", "two", None);
        let third = append(&journal, "user_message", "three", None);

        let result = show(
            &[
                "show".to_owned(),
                second.id.clone(),
                "--around".to_owned(),
                "1".to_owned(),
            ],
            &journal,
        )
        .expect("show");
        assert_eq!(result, vec![first, second, third]);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn search_is_lexical_and_factual() {
        let root = root();
        let journal = Journal::at_root(root.clone());
        append(&journal, "user_message", "Figma migration", Some("/work/a"));
        let wanted = append(&journal, "user_message", "figma gateway", Some("/work/b"));
        append(&journal, "assistant_message", "unrelated", Some("/work/b"));

        let result = search(
            &[
                "search".to_owned(),
                "FIGMA".to_owned(),
                "--cwd".to_owned(),
                "/work/b".to_owned(),
            ],
            &journal,
        )
        .expect("search");
        assert_eq!(result, vec![wanted]);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn reset_changes_global_attention_without_deleting_old_history() {
        let root = root();
        let journal = Journal::at_root(root.clone());
        let mut first = crate::attention::message_event(ChatMessage::user("old".to_owned()))
            .expect("old message");
        first.id = "old".to_owned();
        journal.append(&first).expect("old append");

        let reset_event = reset(&["reset".to_owned()], &journal)
            .expect("reset")
            .pop()
            .expect("reset event");
        let events = journal.read_all().expect("events");
        assert_eq!(latest_attention_head(&events).map(|event| event.id.as_str()), Some(reset_event.id.as_str()));
        assert!(causal_messages(&events, "old").is_ok());
        assert_eq!(events.len(), 2);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn history_output_is_exact_jsonl_event_records() {
        let root = root();
        let journal = Journal::at_root(root.clone());
        let wanted = append(&journal, "user_message", "exact", None);
        let mut output = Vec::new();
        write_events(&mut output, std::slice::from_ref(&wanted)).expect("write");

        let line = std::str::from_utf8(&output).expect("utf8").trim_end();
        let decoded: JournalEvent = serde_json::from_str(line).expect("event");
        assert_eq!(decoded, wanted);
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
