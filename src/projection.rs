use std::collections::HashSet;

use crate::context_budget::{usable_context, NORMAL_OUTPUT_RESERVE_TOKENS};
use crate::model::{estimate_context_tokens, ChatMessage, OBSERVATION_ROLE};

const PROJECTION_BREADCRUMB_PREFIX: &str = "[Lucy context projection:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextProjection {
    pub messages: Vec<ChatMessage>,
    pub omitted_messages: usize,
    pub first_kept_message: usize,
}

pub fn project_context(
    messages: &[ChatMessage],
    context_window: usize,
) -> Result<ContextProjection, String> {
    if messages.is_empty() || context_window == 0 {
        return Ok(ContextProjection {
            messages: messages.to_vec(),
            omitted_messages: 0,
            first_kept_message: 0,
        });
    }

    let target = usable_context(context_window, NORMAL_OUTPUT_RESERVE_TOKENS);
    if target == 0 {
        return Err("context window leaves no usable input budget".to_owned());
    }
    if estimate_context_tokens(messages) <= target {
        return Ok(ContextProjection {
            messages: messages.to_vec(),
            omitted_messages: 0,
            first_kept_message: 0,
        });
    }

    let pruned = crate::tool_pruning::prune_old_tool_outputs(messages);
    if estimate_context_tokens(&pruned) <= target {
        return Ok(ContextProjection {
            messages: pruned,
            omitted_messages: 0,
            first_kept_message: 0,
        });
    }

    let system = messages
        .first()
        .filter(|message| message.role == "system")
        .cloned();
    let body_start = usize::from(system.is_some());
    let turn_starts = (body_start..pruned.len())
        .filter(|index| is_turn_start(&pruned[*index]))
        .collect::<Vec<_>>();

    for start in turn_starts.iter().copied() {
        if let Some(projected) = build_projection(&pruned, system.as_ref(), start, None, target) {
            return Ok(projected);
        }
    }

    let active_start = turn_starts
        .last()
        .copied()
        .unwrap_or(body_start.min(pruned.len()));
    let anchor = pruned.get(active_start).cloned();
    let suffix_candidates = active_start.saturating_add(1)..pruned.len();
    for start in suffix_candidates {
        if !is_safe_suffix_start(&pruned[start]) {
            continue;
        }
        if let Some(projected) =
            build_projection(&pruned, system.as_ref(), start, anchor.as_ref(), target)
        {
            return Ok(projected);
        }
    }

    if let Some(anchor) = anchor {
        let minimal = [system.as_ref(), Some(&anchor)]
            .into_iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        if estimate_context_tokens(&minimal) > target {
            return Err("pinned current request exceeds the usable context budget".to_owned());
        }
    }

    Err("context cannot be projected at a structurally safe boundary".to_owned())
}

fn build_projection(
    messages: &[ChatMessage],
    system: Option<&ChatMessage>,
    start: usize,
    anchor: Option<&ChatMessage>,
    target: usize,
) -> Option<ContextProjection> {
    if start >= messages.len() || !suffix_is_tool_valid(&messages[start..]) {
        return None;
    }

    let omitted_messages = start.saturating_sub(usize::from(system.is_some()));
    let mut projected = Vec::new();
    if let Some(system) = system {
        projected.push(system.clone());
    }
    if let Some(anchor) = anchor {
        if start == 0 || messages.get(start) != Some(anchor) {
            projected.push(anchor.clone());
        }
    }
    if omitted_messages > 0 {
        projected.push(ChatMessage::observation(format!(
            "{PROJECTION_BREADCRUMB_PREFIX} {omitted_messages} earlier messages omitted from active context; canonical history is unchanged.]"
        )));
    }
    projected.extend_from_slice(&messages[start..]);

    (estimate_context_tokens(&projected) <= target).then_some(ContextProjection {
        messages: projected,
        omitted_messages,
        first_kept_message: start,
    })
}

fn is_turn_start(message: &ChatMessage) -> bool {
    message.role == "user" || message.role == OBSERVATION_ROLE
}

fn is_safe_suffix_start(message: &ChatMessage) -> bool {
    is_turn_start(message) || message.role == "assistant"
}

fn suffix_is_tool_valid(messages: &[ChatMessage]) -> bool {
    let mut declared = HashSet::new();
    let mut completed = HashSet::new();
    for message in messages {
        if message.role == "assistant" {
            declared.extend(message.tool_calls.iter().map(|call| call.id.as_str()));
        } else if message.role == "tool" {
            let Some(id) = message.tool_call_id.as_deref() else {
                return false;
            };
            if !declared.contains(id) {
                return false;
            }
            completed.insert(id);
        }
    }
    declared.into_iter().all(|id| completed.contains(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ChatToolCall;

    fn large_text(label: &str, bytes: usize) -> String {
        format!("{label} {}", "x".repeat(bytes))
    }

    fn window_for_target(target: usize) -> usize {
        target + NORMAL_OUTPUT_RESERVE_TOKENS + (target / 50).max(2_048)
    }

    #[test]
    fn projection_is_identity_when_context_fits() {
        let messages = vec![
            ChatMessage::system("system".to_owned()),
            ChatMessage::user("hello".to_owned()),
            ChatMessage::assistant("world".to_owned(), Vec::new()),
        ];

        let projection = project_context(&messages, 128_000).expect("projection");

        assert_eq!(projection.messages, messages);
        assert_eq!(projection.omitted_messages, 0);
        assert_eq!(projection.first_kept_message, 0);
    }

    #[test]
    fn projection_keeps_maximal_recent_complete_turns() {
        let messages = vec![
            ChatMessage::system("system".to_owned()),
            ChatMessage::user("old request".to_owned()),
            ChatMessage::assistant(large_text("old answer", 30_000), Vec::new()),
            ChatMessage::user("recent request".to_owned()),
            ChatMessage::assistant("recent answer".to_owned(), Vec::new()),
        ];
        let target = estimate_context_tokens(&[
            messages[0].clone(),
            ChatMessage::observation(
                "[Lucy context projection: 2 earlier messages omitted from active context; canonical history is unchanged.]"
                    .to_owned(),
            ),
            messages[3].clone(),
            messages[4].clone(),
        ]) + 8;

        let projection = project_context(&messages, window_for_target(target)).expect("projection");

        assert_eq!(projection.first_kept_message, 3);
        assert_eq!(projection.omitted_messages, 2);
        assert_eq!(projection.messages.last(), messages.last());
        assert!(projection.messages.iter().any(|message| {
            message
                .content
                .as_deref()
                .is_some_and(|content| content.starts_with(PROJECTION_BREADCRUMB_PREFIX))
        }));
    }

    #[test]
    fn projection_never_starts_with_an_orphan_tool_result() {
        let call = ChatToolCall {
            id: "call-1".to_owned(),
            name: "cmd".to_owned(),
            arguments: r#"{"command":"pwd"}"#.to_owned(),
        };
        let messages = vec![
            ChatMessage::system("system".to_owned()),
            ChatMessage::user("old".to_owned()),
            ChatMessage::assistant(large_text("old", 20_000), Vec::new()),
            ChatMessage::user("active".to_owned()),
            ChatMessage::assistant(String::new(), vec![call]),
            ChatMessage::tool(
                "call-1".to_owned(),
                "cmd".to_owned(),
                large_text("result", 20_000),
            ),
            ChatMessage::assistant("done".to_owned(), Vec::new()),
        ];

        let target = estimate_context_tokens(&[
            messages[0].clone(),
            messages[3].clone(),
            ChatMessage::observation(
                "[Lucy context projection: 3 earlier messages omitted from active context; canonical history is unchanged.]"
                    .to_owned(),
            ),
            messages[6].clone(),
        ]) + 64;
        let projection = project_context(&messages, window_for_target(target)).expect("projection");

        assert!(suffix_is_tool_valid(&projection.messages));
        assert_ne!(projection.messages.get(1).map(|message| message.role.as_str()), Some("tool"));
    }

    #[test]
    fn projection_fails_loudly_when_pinned_request_alone_is_too_large() {
        let messages = vec![
            ChatMessage::system("system".to_owned()),
            ChatMessage::user(large_text("request", 80_000)),
            ChatMessage::assistant(large_text("answer", 80_000), Vec::new()),
        ];
        let target = estimate_context_tokens(&[messages[0].clone()]) + 100;

        assert_eq!(
            project_context(&messages, window_for_target(target)).expect_err("must fail"),
            "pinned current request exceeds the usable context budget"
        );
    }

    #[test]
    fn projection_is_deterministic() {
        let messages = vec![
            ChatMessage::system("system".to_owned()),
            ChatMessage::user("one".to_owned()),
            ChatMessage::assistant(large_text("one", 30_000), Vec::new()),
            ChatMessage::user("two".to_owned()),
            ChatMessage::assistant("two".to_owned(), Vec::new()),
        ];
        let window = 24_000;

        assert_eq!(
            project_context(&messages, window),
            project_context(&messages, window)
        );
    }
}
