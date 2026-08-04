import re
from pathlib import Path

intra_path = Path("src/intra_turn.rs")
intra = intra_path.read_text()
old_payload = '''    Ok(vec![
        ChatMessage::system(boot_system_prompt.to_owned()),
        ChatMessage::system(SUMMARY_SYSTEM_PROMPT.to_owned()),
        ChatMessage::user(format!(
            "<lucy_compaction_input_json>\\n{}\\n</lucy_compaction_input_json>",
            serde_json::to_string(&payload)
                .map_err(|error| format!("unable to encode compaction input: {error}"))?
        )),
    ])'''
new_payload = '''    let encoded = serde_json::to_string(&vec![payload])
        .map_err(|error| format!("unable to encode compaction input: {error}"))?;
    Ok(vec![
        ChatMessage::system(boot_system_prompt.to_owned()),
        ChatMessage::system(SUMMARY_SYSTEM_PROMPT.to_owned()),
        ChatMessage::user(format!(
            "<lucy_compaction_input_json>\\n<discarded_history_json>\\n{encoded}\\n</discarded_history_json>\\n</lucy_compaction_input_json>"
        )),
    ])'''
if intra.count(old_payload) != 1:
    raise SystemExit("unexpected split-turn payload encoding")
intra_path.write_text(intra.replace(old_payload, new_payload))

pruning_path = Path("src/tool_pruning.rs")
pruning = pruning_path.read_text()
old_assert = '        assert_eq!(messages[0].content.as_deref(), Some("x".repeat(100_000).as_str()));'
new_assert = '        assert_eq!(messages[0].content.as_deref().map(str::len), Some(100_000));'
if pruning.count(old_assert) != 1:
    raise SystemExit("unexpected raw-history pruning assertion")
pruning_path.write_text(pruning.replace(old_assert, new_assert))

session_path = Path("src/session.rs")
session = session_path.read_text()
method_marker = "    /// Append a semantic message while maintaining one explicit logical turn."
provider_method = '''    pub fn provider_messages(&self) -> Vec<ChatMessage> {
        crate::tool_pruning::prune_old_tool_outputs(&self.inner.provider_messages())
    }

'''
if session.count(method_marker) != 1:
    raise SystemExit("unexpected session provider-message insertion point")
session = session.replace(method_marker, provider_method + method_marker)
test = '''
    #[test]
    fn pruned_provider_context_is_stable_across_resume_without_mutating_raw_history() {
        let (home, mut session) = session();
        session
            .append_message(ChatMessage::user("run a large command".to_owned()))
            .expect("user");
        session
            .append_message(ChatMessage::assistant(
                "running".to_owned(),
                vec![crate::model::ChatToolCall {
                    id: "large".to_owned(),
                    name: "cmd".to_owned(),
                    arguments: "{}".to_owned(),
                }],
            ))
            .expect("assistant");
        session
            .append_message(ChatMessage::tool(
                "large".to_owned(),
                "cmd".to_owned(),
                "x".repeat(100_000),
            ))
            .expect("tool");
        let id = session.id.clone();
        let first = session.provider_messages();
        assert_eq!(
            session
                .messages
                .last()
                .and_then(|message| message.content.as_deref())
                .map(str::len),
            Some(100_000)
        );
        drop(session);

        let resumed = Session::resume(&home, &id).expect("resume");
        assert_eq!(resumed.provider_messages(), first);
        assert_eq!(
            resumed
                .messages
                .last()
                .and_then(|message| message.content.as_deref())
                .map(str::len),
            Some(100_000)
        );
        drop(resumed);
        fs::remove_dir_all(home).expect("cleanup");
    }
'''
end = session.rfind("\n}")
if end < 0:
    raise SystemExit("unexpected session wrapper test ending")
session_path.write_text(session[:end] + test + session[end:])

app_path = Path("src/app.rs")
app = app_path.read_text()
old_import = "use crate::model::{estimate_context_tokens, estimate_message_tokens, ChatMessage, ChatToolCall};"
new_import = "use crate::model::{estimate_context_tokens, ChatMessage, ChatToolCall};"
if app.count(old_import) != 1:
    raise SystemExit("unexpected model import")
app = app.replace(old_import, new_import)
constants = [
    "const AUTO_COMPACTION_THRESHOLD_PERCENT: usize = 95;\n",
    "const COMPACTION_SYSTEM_PROMPT: &str = \"You are compacting a coding-agent conversation. Produce a concise, factual continuation summary. Preserve the user's goals, explicit decisions, constraints, files and code changes, commands and results, current implementation state, unresolved work, and exact identifiers that future turns need. Do not invent facts. Return only the summary text; do not call tools.\";\n",
]
for constant in constants:
    if app.count(constant) != 1:
        raise SystemExit(f"unexpected compaction constant: {constant[:40]}")
    app = app.replace(constant, "")

app, trigger_count = re.subn(
    r"fn should_compact_context\(context_tokens: usize, context_window: usize\) -> bool \{\n"
    r"\s*context_window > 0\n"
    r"\s*&& context_tokens as u128 \* 100\n"
    r"\s*>= context_window as u128 \* AUTO_COMPACTION_THRESHOLD_PERCENT as u128\n"
    r"\}",
    "fn should_compact_context(context_tokens: usize, context_window: usize) -> bool {\n"
    "    crate::context_budget::should_compact(context_tokens, context_window)\n"
    "}",
    app,
    count=1,
)
boundary_pattern = re.compile(
    r"fn find_compaction_boundary\(.*?\n\}\n\n(?=impl Harness)",
    re.DOTALL,
)
boundary_replacement = '''fn find_compaction_plan(
    messages: &[ChatMessage],
    previous_boundary: Option<usize>,
) -> Option<crate::intra_turn::CompactionPlan> {
    let pruned = crate::tool_pruning::prune_old_tool_outputs(messages);
    crate::intra_turn::find_compaction_plan(
        &pruned,
        previous_boundary,
        COMPACTION_KEEP_RECENT_TOKENS,
    )
}

fn find_compaction_boundary(
    messages: &[ChatMessage],
    previous_boundary: Option<usize>,
) -> Option<usize> {
    find_compaction_plan(messages, previous_boundary).map(|plan| plan.boundary)
}

'''
app, boundary_count = boundary_pattern.subn(boundary_replacement, app, count=1)

method_pattern = re.compile(
    r"    fn compaction_boundary\(&self\) -> Option<usize> \{.*?\n"
    r"    \}\n\n"
    r"    fn compact_context<S: EventSink>\(.*?\n"
    r"    \}\n\n(?=    pub\(crate\) fn handle_message)",
    re.DOTALL,
)
method_replacement = '''    fn compaction_plan(&self) -> Option<crate::intra_turn::CompactionPlan> {
        let latest_boundary = self
            .session
            .history
            .iter()
            .rev()
            .find_map(|record| match record {
                crate::session::SessionHistoryRecord::Compaction(compaction) => {
                    Some(compaction.first_kept_message)
                }
                _ => None,
            });
        find_compaction_plan(&self.session.messages, latest_boundary)
    }

    fn compact_context<S: EventSink>(
        &mut self,
        sink: &mut S,
        cancellation: Option<&crate::cancellation::CancellationToken>,
        tokens_before: usize,
    ) -> Result<usize, String> {
        let Some(plan) = self.compaction_plan() else {
            return Err("context cannot be compacted at a structurally safe boundary".to_owned());
        };
        let Some(cancellation) = cancellation else {
            return Err("context compaction requires a cancellable turn".to_owned());
        };
        let (previous_boundary, previous_summary) = self
            .session
            .history
            .iter()
            .rev()
            .find_map(|record| match record {
                crate::session::SessionHistoryRecord::Compaction(compaction) => Some((
                    Some(compaction.first_kept_message),
                    Some(compaction.summary.clone()),
                )),
                _ => None,
            })
            .unwrap_or((None, None));
        sink.compaction_started()
            .map_err(|error| format!("unable to emit compaction state: {error}"))?;
        let summary_messages = crate::intra_turn::prepare_summary_messages(
            &self.session.boot_system_prompt,
            previous_summary.as_deref(),
            &self.session.messages,
            previous_boundary,
            plan,
        )?;
        let summary = match self
            .provider
            .summarize_prepared(summary_messages, cancellation)
        {
            Ok(summary) => redact_secret(&summary, Some(self.provider.api_key().as_str())),
            Err(error) if cancellation.is_cancelled() || error.is_cancelled() => {
                self.interrupt(sink, PROVIDER_PHASE, "", &[], Vec::new())?;
                return Ok(plan.boundary);
            }
            Err(error) => return Err(format!("unable to compact context: {error}")),
        };
        self.session
            .append_compaction(summary, plan.boundary, tokens_before)
            .map_err(|error| format!("unable to persist context compaction: {error}"))?;
        let tokens_after = estimate_context_tokens(&self.session.provider_messages());
        sink.compaction_finished(tokens_before, tokens_after)
            .map_err(|error| format!("unable to emit compaction state: {error}"))?;
        Ok(plan.boundary)
    }

'''
app, method_count = method_pattern.subn(method_replacement, app, count=1)

old_compacted = "        let mut compacted_for_turn = false;"
if app.count(old_compacted) != 1:
    raise SystemExit("unexpected per-turn compaction guard")
app = app.replace(old_compacted, "        let mut last_compaction_boundary = None;")
old_loop = '''            if !compacted_for_turn && self.should_compact(&messages) {
                self.compact_context(sink, cancellation, tokens_before)?;
                compacted_for_turn = true;
                messages = self.session.provider_messages();
            }'''
new_loop = '''            if self.should_compact(&messages) {
                let boundary = self.compact_context(sink, cancellation, tokens_before)?;
                if last_compaction_boundary == Some(boundary) {
                    return Err("context compaction did not advance its boundary".to_owned());
                }
                last_compaction_boundary = Some(boundary);
                messages = self.session.provider_messages();
            }'''
if app.count(old_loop) != 1:
    raise SystemExit("unexpected compaction loop")
app = app.replace(old_loop, new_loop)

app, auto_test_count = re.subn(
    r"(?s)    #\[test\]\n"
    r"    fn auto_compaction_triggers_at_or_above_ninety_five_percent_only\(\) \{.*?\n"
    r"    \}\n",
    "    #[test]\n"
    "    fn auto_compaction_reserves_output_and_estimation_headroom() {\n"
    "        assert!(!should_compact_context(109_055, 128_000));\n"
    "        assert!(should_compact_context(109_056, 128_000));\n"
    "        assert!(should_compact_context(110_000, 128_000));\n"
    "        assert!(!should_compact_context(100, 0));\n"
    "    }\n",
    app,
    count=1,
)
old_boundary_asserts = '''        assert_eq!(find_compaction_boundary(&messages, None), Some(2));
        assert_eq!(find_compaction_boundary(&messages, Some(2)), None);'''
new_boundary_asserts = '''        assert_eq!(find_compaction_boundary(&messages, None), Some(3));
        assert_eq!(find_compaction_boundary(&messages, Some(3)), None);'''
if app.count(old_boundary_asserts) != 1:
    raise SystemExit("unexpected compaction boundary test")
app = app.replace(old_boundary_asserts, new_boundary_asserts)
if (trigger_count, boundary_count, method_count, auto_test_count) != (1, 1, 1, 1):
    raise SystemExit(
        f"unexpected app patch counts: trigger={trigger_count}, boundary={boundary_count}, "
        f"method={method_count}, auto_test={auto_test_count}"
    )
app_path.write_text(app)
