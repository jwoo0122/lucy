use std::collections::{HashMap, HashSet};

use crate::journal::JournalEvent;
use crate::model::ChatMessage;

pub const MESSAGE_EVENT_KIND: &str = "message";

/// Encode an exact provider/chat message as a factual journal event. Routing
/// and causal provenance stay in JournalEvent's top-level fields rather than
/// being inferred from the message text.
pub fn message_event(message: ChatMessage) -> Result<JournalEvent, String> {
    let payload =
        serde_json::to_value(message).map_err(|_| "unable to encode journal message".to_owned())?;
    JournalEvent::new(MESSAGE_EVENT_KIND, payload)
}

/// Add factual transport provenance to a message event. `source_id` is a
/// routing key, not a memory namespace: callers may still recall any journal
/// event regardless of source.
pub fn source_message_event(
    message: ChatMessage,
    surface: &str,
    source_id: &str,
    parent_id: Option<&str>,
) -> Result<JournalEvent, String> {
    if surface.trim().is_empty() || source_id.trim().is_empty() {
        return Err("attention source must not be empty".to_owned());
    }
    let mut event = message_event(message)?;
    event.surface = Some(surface.to_owned());
    event.source_id = Some(source_id.to_owned());
    event.parent_id = parent_id.map(str::to_owned);
    Ok(event)
}

/// Derive the durable attention head for one factual transport source from the
/// journal itself. No second cursor store is required: the most recently
/// committed event carrying the same `(surface, source_id)` is the source head.
pub fn latest_source_head<'a>(
    events: &'a [JournalEvent],
    surface: &str,
    source_id: &str,
) -> Option<&'a JournalEvent> {
    events.iter().rev().find(|event| {
        event.surface.as_deref() == Some(surface) && event.source_id.as_deref() == Some(source_id)
    })
}

/// Follow one explicit causal head backwards through parent_id links, then
/// return the chain in chronological causal order. Global append order is not
/// used to infer topical/thread relationships.
pub fn causal_events<'a>(
    events: &'a [JournalEvent],
    head_id: &str,
) -> Result<Vec<&'a JournalEvent>, String> {
    if head_id.trim().is_empty() {
        return Err("attention head must not be empty".to_owned());
    }

    let mut by_id = HashMap::with_capacity(events.len());
    for event in events {
        if by_id.insert(event.id.as_str(), event).is_some() {
            return Err("journal contains duplicate event ids".to_owned());
        }
    }

    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = Some(head_id);
    while let Some(id) = cursor {
        if !seen.insert(id) {
            return Err("journal causal chain contains a cycle".to_owned());
        }
        let event = by_id
            .get(id)
            .copied()
            .ok_or_else(|| "journal causal chain references a missing event".to_owned())?;
        chain.push(event);
        cursor = event.parent_id.as_deref();
    }
    chain.reverse();
    Ok(chain)
}

/// Decode only exact message events from a causal chain. Non-message events
/// remain part of the journal's provenance but do not become provider messages
/// unless an explicit projection policy chooses to represent them.
pub fn causal_messages(events: &[JournalEvent], head_id: &str) -> Result<Vec<ChatMessage>, String> {
    causal_events(events, head_id)?
        .into_iter()
        .filter(|event| event.kind == MESSAGE_EVENT_KIND)
        .map(|event| {
            serde_json::from_value::<ChatMessage>(event.payload.clone())
                .map_err(|_| "journal message event has invalid payload".to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linked_message(id: &str, parent_id: Option<&str>, text: &str) -> JournalEvent {
        let mut event = message_event(ChatMessage::user(text.to_owned())).expect("message event");
        event.id = id.to_owned();
        event.parent_id = parent_id.map(str::to_owned);
        event
    }

    fn sourced_message(
        id: &str,
        surface: &str,
        source_id: &str,
        parent_id: Option<&str>,
        text: &str,
    ) -> JournalEvent {
        let mut event = source_message_event(
            ChatMessage::user(text.to_owned()),
            surface,
            source_id,
            parent_id,
        )
        .expect("source message");
        event.id = id.to_owned();
        event
    }

    #[test]
    fn interleaved_global_history_does_not_mix_causal_attention() {
        let a1 = linked_message("a1", None, "A one");
        let b1 = linked_message("b1", None, "B one");
        let a2 = linked_message("a2", Some("a1"), "A two");
        let b2 = linked_message("b2", Some("b1"), "B two");
        let events = vec![a1, b1, a2, b2];

        let a = causal_messages(&events, "a2").expect("A attention");
        let b = causal_messages(&events, "b2").expect("B attention");

        assert_eq!(
            a.iter()
                .filter_map(|message| message.content.as_deref())
                .collect::<Vec<_>>(),
            vec!["A one", "A two"]
        );
        assert_eq!(
            b.iter()
                .filter_map(|message| message.content.as_deref())
                .collect::<Vec<_>>(),
            vec!["B one", "B two"]
        );
    }

    #[test]
    fn source_heads_are_derived_independently_from_interleaved_journal_order() {
        let a1 = sourced_message("a1", "telegram", "100", None, "A one");
        let b1 = sourced_message("b1", "telegram", "200", None, "B one");
        let a2 = sourced_message("a2", "telegram", "100", Some("a1"), "A two");
        let b2 = sourced_message("b2", "telegram", "200", Some("b1"), "B two");
        let events = vec![a1, b1, a2, b2];

        assert_eq!(
            latest_source_head(&events, "telegram", "100").map(|event| event.id.as_str()),
            Some("a2")
        );
        assert_eq!(
            latest_source_head(&events, "telegram", "200").map(|event| event.id.as_str()),
            Some("b2")
        );
    }

    #[test]
    fn new_root_resets_source_attention_without_deleting_old_branch() {
        let old1 = sourced_message("old1", "tui", "main", None, "old one");
        let old2 = sourced_message("old2", "tui", "main", Some("old1"), "old two");
        let fresh = sourced_message("fresh", "tui", "main", None, "fresh root");
        let events = vec![old1, old2, fresh];

        let head = latest_source_head(&events, "tui", "main").expect("head");
        assert_eq!(head.id, "fresh");
        assert_eq!(
            causal_messages(&events, &head.id)
                .expect("fresh attention")
                .iter()
                .filter_map(|message| message.content.as_deref())
                .collect::<Vec<_>>(),
            vec!["fresh root"]
        );
        assert_eq!(
            causal_messages(&events, "old2")
                .expect("old branch remains")
                .iter()
                .filter_map(|message| message.content.as_deref())
                .collect::<Vec<_>>(),
            vec!["old one", "old two"]
        );
    }

    #[test]
    fn causal_chain_preserves_non_message_provenance_without_injecting_it() {
        let first = linked_message("m1", None, "before");
        let mut tool_observation =
            JournalEvent::new("transport_delivery", serde_json::json!({"update_id": 42}))
                .expect("event");
        tool_observation.id = "p1".to_owned();
        tool_observation.parent_id = Some("m1".to_owned());
        let second = linked_message("m2", Some("p1"), "after");
        let events = vec![first, tool_observation, second];

        assert_eq!(causal_events(&events, "m2").expect("chain").len(), 3);
        assert_eq!(
            causal_messages(&events, "m2")
                .expect("messages")
                .iter()
                .filter_map(|message| message.content.as_deref())
                .collect::<Vec<_>>(),
            vec!["before", "after"]
        );
    }

    #[test]
    fn missing_parent_is_loud() {
        let event = linked_message("m1", Some("gone"), "hello");
        assert_eq!(
            causal_events(&[event], "m1").expect_err("missing parent"),
            "journal causal chain references a missing event"
        );
    }

    #[test]
    fn causal_cycle_is_loud() {
        let first = linked_message("m1", Some("m2"), "one");
        let second = linked_message("m2", Some("m1"), "two");
        assert_eq!(
            causal_events(&[first, second], "m1").expect_err("cycle"),
            "journal causal chain contains a cycle"
        );
    }

    #[test]
    fn duplicate_ids_are_loud_even_off_the_selected_branch() {
        let first = linked_message("same", None, "one");
        let duplicate = linked_message("same", None, "two");
        assert_eq!(
            causal_events(&[first, duplicate], "same").expect_err("duplicate"),
            "journal contains duplicate event ids"
        );
    }

    #[test]
    fn message_event_round_trips_exact_message_shape() {
        let original = ChatMessage::assistant(
            "answer".to_owned(),
            vec![crate::model::ChatToolCall {
                id: "call-1".to_owned(),
                name: "cmd".to_owned(),
                arguments: r#"{"command":"pwd"}"#.to_owned(),
            }],
        );
        let event = message_event(original.clone()).expect("event");

        assert_eq!(event.kind, MESSAGE_EVENT_KIND);
        assert_eq!(
            causal_messages(std::slice::from_ref(&event), &event.id).expect("decode"),
            vec![original]
        );
    }
}
