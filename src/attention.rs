use std::collections::{HashMap, HashSet};

use crate::journal::JournalEvent;
use crate::model::ChatMessage;

pub const MESSAGE_EVENT_KIND: &str = "message";
pub const ATTENTION_RESET_KIND: &str = "attention_reset";

/// Encode an exact provider/chat message as a factual journal event. Routing
/// and causal provenance stay in JournalEvent's top-level fields rather than
/// being inferred from the message text.
pub fn message_event(message: ChatMessage) -> Result<JournalEvent, String> {
    let payload =
        serde_json::to_value(message).map_err(|_| "unable to encode journal message".to_owned())?;
    JournalEvent::new(MESSAGE_EVENT_KIND, payload)
}

/// Add factual transport provenance to a message event. `surface` and
/// `source_id` describe where the event entered or should be routed; they do
/// not select a separate memory or attention cursor.
pub fn routed_message_event(
    message: ChatMessage,
    surface: &str,
    source_id: &str,
    parent_id: Option<&str>,
) -> Result<JournalEvent, String> {
    if surface.trim().is_empty() || source_id.trim().is_empty() {
        return Err("message routing provenance must not be empty".to_owned());
    }
    let mut event = message_event(message)?;
    event.surface = Some(surface.to_owned());
    event.source_id = Some(source_id.to_owned());
    event.parent_id = parent_id.map(str::to_owned);
    Ok(event)
}

/// Build a global attention reset while preserving the transport that issued
/// it as factual provenance. A reset is one new root in Lucy's single history,
/// not a source-local session boundary.
pub fn attention_reset_event(surface: &str, source_id: &str) -> Result<JournalEvent, String> {
    if surface.trim().is_empty() || source_id.trim().is_empty() {
        return Err("attention reset routing provenance must not be empty".to_owned());
    }
    let mut event = JournalEvent::new(ATTENTION_RESET_KIND, serde_json::json!({}))?;
    event.surface = Some(surface.to_owned());
    event.source_id = Some(source_id.to_owned());
    event.parent_id = None;
    Ok(event)
}

fn advances_attention(event: &JournalEvent) -> bool {
    matches!(event.kind.as_str(), MESSAGE_EVENT_KIND | ATTENTION_RESET_KIND)
}

/// Derive Lucy's one durable attention head from the canonical journal. The
/// latest committed message or explicit reset owns current attention regardless
/// of which transport produced it. Provenance-only events do not move the head.
pub fn latest_attention_head(events: &[JournalEvent]) -> Option<&JournalEvent> {
    events.iter().rev().find(|event| advances_attention(event))
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

    fn routed_message(
        id: &str,
        surface: &str,
        source_id: &str,
        parent_id: Option<&str>,
        text: &str,
    ) -> JournalEvent {
        let mut event = routed_message_event(
            ChatMessage::user(text.to_owned()),
            surface,
            source_id,
            parent_id,
        )
        .expect("routed message");
        event.id = id.to_owned();
        event
    }

    #[test]
    fn explicit_causal_branches_remain_isolated_when_selected_directly() {
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
    fn transport_switches_share_one_attention_head() {
        let tui = routed_message("m1", "tui", "main", None, "one");
        let telegram = routed_message("m2", "telegram", "100", Some("m1"), "two");
        let tui_again = routed_message("m3", "tui", "main", Some("m2"), "three");
        let events = vec![tui, telegram, tui_again];

        let head = latest_attention_head(&events).expect("global head");
        assert_eq!(head.id, "m3");
        assert_eq!(
            causal_messages(&events, &head.id)
                .expect("messages")
                .iter()
                .filter_map(|message| message.content.as_deref())
                .collect::<Vec<_>>(),
            vec!["one", "two", "three"]
        );
    }

    #[test]
    fn routing_metadata_never_partitions_attention() {
        let first = routed_message("m1", "telegram", "100", None, "first");
        let second = routed_message("m2", "telegram", "200", Some("m1"), "second");
        let events = vec![first, second];

        let head = latest_attention_head(&events).expect("head");
        assert_eq!(head.id, "m2");
        assert_eq!(head.source_id.as_deref(), Some("200"));
        assert_eq!(
            causal_messages(&events, &head.id)
                .expect("messages")
                .iter()
                .filter_map(|message| message.content.as_deref())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }

    #[test]
    fn reset_is_global_even_when_issued_from_one_transport() {
        let old1 = routed_message("old1", "tui", "main", None, "old one");
        let old2 = routed_message("old2", "telegram", "100", Some("old1"), "old two");
        let mut reset = attention_reset_event("tui", "main").expect("reset");
        reset.id = "reset".to_owned();
        let fresh = routed_message("fresh", "telegram", "100", Some("reset"), "fresh root");
        let events = vec![old1, old2, reset, fresh];

        let head = latest_attention_head(&events).expect("head");
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
    fn provenance_only_events_do_not_steal_attention() {
        let message = routed_message("m1", "tui", "main", None, "hello");
        let mut delivery =
            JournalEvent::new("transport_delivery", serde_json::json!({"update_id": 42}))
                .expect("delivery");
        delivery.id = "delivery".to_owned();
        delivery.parent_id = Some("m1".to_owned());
        let events = vec![message, delivery];

        assert_eq!(
            latest_attention_head(&events).map(|event| event.id.as_str()),
            Some("m1")
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
