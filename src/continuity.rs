use crate::attention::{
    attention_reset_event, causal_messages, latest_attention_head, routed_message_event,
};
use crate::journal::JournalEvent;
use crate::model::ChatMessage;

/// A disposable view over Lucy's one current attention in the global journal.
///
/// The view owns no persistent cursor and is not bound to a transport. Routing
/// provenance is supplied only when constructing the next event.
pub struct AttentionView<'a> {
    events: &'a [JournalEvent],
}

impl<'a> AttentionView<'a> {
    pub fn new(events: &'a [JournalEvent]) -> Self {
        Self { events }
    }

    pub fn head(&self) -> Option<&'a JournalEvent> {
        latest_attention_head(self.events)
    }

    pub fn messages(&self) -> Result<Vec<ChatMessage>, String> {
        match self.head() {
            Some(head) => causal_messages(self.events, &head.id),
            None => Ok(Vec::new()),
        }
    }

    /// Build the next exact message event on Lucy's one attention chain. The
    /// transport fields are provenance/routing only; parentage always uses the
    /// global current head.
    pub fn next_message(
        &self,
        message: ChatMessage,
        surface: &str,
        source_id: &str,
    ) -> Result<JournalEvent, String> {
        routed_message_event(
            message,
            surface,
            source_id,
            self.head().map(|event| event.id.as_str()),
        )
    }

    /// Build one global attention reset, recording only which transport issued
    /// it. Once appended, every transport continues from this new root.
    pub fn reset(&self, surface: &str, source_id: &str) -> Result<JournalEvent, String> {
        attention_reset_event(surface, source_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append_next(events: &mut Vec<JournalEvent>, surface: &str, source_id: &str, text: &str) {
        let view = AttentionView::new(events);
        let event = view
            .next_message(ChatMessage::user(text.to_owned()), surface, source_id)
            .expect("next message");
        events.push(event);
    }

    #[test]
    fn discarded_view_recovers_one_head_from_journal_only() {
        let mut events = Vec::new();
        append_next(&mut events, "tui", "main", "one");
        append_next(&mut events, "telegram", "100", "two");
        append_next(&mut events, "tui", "main", "three");

        let rebuilt = AttentionView::new(&events);
        assert_eq!(
            rebuilt
                .messages()
                .expect("messages")
                .iter()
                .filter_map(|message| message.content.as_deref())
                .collect::<Vec<_>>(),
            vec!["one", "two", "three"]
        );
    }

    #[test]
    fn reset_from_one_transport_resets_global_attention_without_memory_deletion() {
        let mut events = Vec::new();
        append_next(&mut events, "tui", "main", "old one");
        append_next(&mut events, "telegram", "100", "old two");
        let old_head = events.last().expect("old head").id.clone();

        let reset = AttentionView::new(&events)
            .reset("tui", "main")
            .expect("reset");
        assert_eq!(reset.parent_id, None);
        events.push(reset);
        append_next(&mut events, "telegram", "100", "fresh");

        let current = AttentionView::new(&events);
        assert_eq!(
            current
                .messages()
                .expect("current messages")
                .iter()
                .filter_map(|message| message.content.as_deref())
                .collect::<Vec<_>>(),
            vec!["fresh"]
        );
        assert_eq!(
            causal_messages(&events, &old_head)
                .expect("old memory")
                .iter()
                .filter_map(|message| message.content.as_deref())
                .collect::<Vec<_>>(),
            vec!["old one", "old two"]
        );
    }

    #[test]
    fn routing_metadata_changes_without_changing_attention_identity() {
        let mut events = Vec::new();
        append_next(&mut events, "telegram", "100", "chat one");
        append_next(&mut events, "telegram", "200", "chat two");

        let view = AttentionView::new(&events);
        let head = view.head().expect("head");
        assert_eq!(head.source_id.as_deref(), Some("200"));
        assert_eq!(
            view.messages()
                .expect("messages")
                .iter()
                .filter_map(|message| message.content.as_deref())
                .collect::<Vec<_>>(),
            vec!["chat one", "chat two"]
        );
    }

    #[test]
    fn next_message_always_parents_to_global_head() {
        let mut events = Vec::new();
        append_next(&mut events, "tui", "main", "first");
        let expected_parent = events.last().expect("first").id.clone();

        let next = AttentionView::new(&events)
            .next_message(ChatMessage::user("second".to_owned()), "telegram", "100")
            .expect("next");

        assert_eq!(next.parent_id.as_deref(), Some(expected_parent.as_str()));
        assert_eq!(next.surface.as_deref(), Some("telegram"));
        assert_eq!(next.source_id.as_deref(), Some("100"));
    }
}
