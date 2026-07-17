const LEGACY_MARKER: &str = "[REDACTED]";
const PRINTABLE_ASCII_START: u32 = 0x21;
const PRINTABLE_ASCII_END: u32 = 0x7e;
const TUI_PROTECTED_CHARACTERS: &[char] = &['>', '─', '│', '┌', '┐', '└', '┘'];

const STRUCTURAL_KEYS: &[&str] = &[
    "additionalProperties",
    "api_key_env",
    "arguments",
    "assistant_text",
    "base_url",
    "boot_system_prompt",
    "command",
    "cmd",
    "content",
    "created_at",
    "cwd",
    "description",
    "error",
    "exit_code",
    "first_message",
    "function",
    "id",
    "last_message",
    "llm",
    "message",
    "messages",
    "model",
    "name",
    "parameters",
    "properties",
    "phase",
    "reason",
    "record",
    "reasoning_details",
    "required",
    "resumed",
    "result",
    "role",
    "session_id",
    "stderr",
    "stderr_truncated",
    "stdout",
    "stdout_truncated",
    "stream",
    "system_prompt",
    "text",
    "timestamp",
    "timed_out",
    "canceled",
    "tool_call_id",
    "tool_calls",
    "tool_results",
    "tools",
    "type",
    "updated_at",
    "version",
];

// These literals are emitted by Lucy-owned JSON/session/protocol schemas or by
// fixed typed JSON values. A credential is unsafe when the complete credential
// is contained in one of them; a longer credential that merely contains a
// field name remains valid.
const PROTECTED_LITERALS: &[&str] = &[
    "0",
    "1",
    "assistant",
    "assistant_delta",
    "cmd",
    "Execute a finite shell command in the session starting directory.",
    "error",
    "false",
    "function",
    "interruption",
    "message",
    "null",
    "object",
    "provider",
    "provider_stream",
    "session",
    "session_metadata",
    "string",
    "system",
    "tool",
    "tool_call",
    "tool_result",
    "true",
    "turn_end",
    "turn_interrupted",
    "user",
    "user_cancelled",
];

pub(crate) fn redact_secret(text: &str, secret: Option<&str>) -> String {
    let Some(secret) = secret.filter(|secret| !secret.is_empty()) else {
        return text.to_owned();
    };

    let marker = redaction_marker(secret).unwrap_or_default();
    let redacted = text.replace(secret, &marker);
    if serialized_contains_secret(&redacted, secret) {
        // JSON escaping can introduce a credential substring that was not
        // present in the in-memory string, for example `u0` in `\\u0000`.
        marker
    } else {
        redacted
    }
}

fn serialized_contains_secret(text: &str, secret: &str) -> bool {
    serde_json::to_string(text)
        .ok()
        .is_some_and(|serialized| serialized.contains(secret))
}

pub(crate) fn redaction_marker(secret: &str) -> Option<String> {
    if secret.is_empty() {
        return None;
    }

    if marker_is_safe(LEGACY_MARKER, secret)
        && LEGACY_MARKER.len() <= secret.len()
        && LEGACY_MARKER.chars().count() <= secret.chars().count()
    {
        return Some(LEGACY_MARKER.to_owned());
    }

    let character = (PRINTABLE_ASCII_START..=PRINTABLE_ASCII_END)
        .filter_map(char::from_u32)
        .find(|character| marker_character_is_safe(*character, secret))
        .or_else(|| find_private_use_marker(secret))?;
    Some(character.to_string())
}

fn marker_is_safe(marker: &str, secret: &str) -> bool {
    marker.chars().all(|character| !secret.contains(character)) && !marker.contains(secret)
}

fn marker_character_is_safe(character: char, secret: &str) -> bool {
    character.len_utf8() <= secret.len() && !secret.contains(character)
}

fn find_private_use_marker(secret: &str) -> Option<char> {
    [(0xe000, 0xf8ff), (0xf0000, 0xffffd), (0x100000, 0x10fffd)]
        .into_iter()
        .flat_map(|(start, end)| start..=end)
        .filter_map(char::from_u32)
        .find(|character| marker_character_is_safe(*character, secret))
}

pub(crate) fn is_structural_key(key: &str) -> bool {
    STRUCTURAL_KEYS.contains(&key)
}

pub(crate) fn conflicts_with_tui_literal(secret: &str) -> bool {
    secret
        .chars()
        .any(|character| TUI_PROTECTED_CHARACTERS.contains(&character))
}

pub(crate) fn conflicts_with_protected_literal(secret: &str) -> bool {
    if secret.is_empty() {
        return false;
    }

    // These values can occur in fixed JSON syntax or generated typed fields.
    // They cannot be redacted without changing the wire format or field type.
    let unsafe_syntax = secret.chars().any(|character| {
        character.is_control()
            || matches!(character, '"' | '\\' | '{' | '}' | '[' | ']' | ':' | ',')
    });
    let numeric_only = secret.chars().all(|character| character.is_ascii_digit());
    unsafe_syntax
        || numeric_only
        || STRUCTURAL_KEYS
            .iter()
            .chain(PROTECTED_LITERALS.iter())
            .any(|literal| literal.contains(secret))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_substrings_of_fixed_literals_but_allows_long_keys() {
        for secret in [
            "session",
            "tool",
            "cmd",
            "command",
            "0",
            "123",
            ":",
            "[REDACTED]",
        ] {
            assert!(
                conflicts_with_protected_literal(secret),
                "fixed literal collision should be rejected: {secret}"
            );
        }
        assert!(!conflicts_with_protected_literal("provider-secret"));
        assert!(!conflicts_with_protected_literal("long-command-marker"));
    }

    #[test]
    fn rejects_terminal_ui_literal_collisions() {
        for secret in [">", "─", "> ", "┌─"] {
            assert!(conflicts_with_tui_literal(secret), "secret: {secret}");
        }
    }

    #[test]
    fn chooses_a_marker_that_cannot_reintroduce_collision_keys() {
        for secret in ["REDACTED", "[REDACTED]"] {
            let marker = redaction_marker(secret).expect("marker");
            assert_eq!(marker.chars().count(), 1);
            assert!(!marker.contains(secret));
            assert!(!marker.chars().any(|character| secret.contains(character)));
            assert_eq!(redact_secret(secret, Some(secret)), marker);
        }
    }

    #[test]
    fn redacts_secrets_introduced_by_json_escaping() {
        assert!(!redact_secret("\0", Some("u0")).contains("u0"));
        assert!(!redact_secret("\n0", Some("n0")).contains("n0"));
    }

    #[test]
    fn marker_replacement_does_not_expand_bytes() {
        for secret in ["x", "secret", "provider-secret", "é"] {
            let input = secret.repeat(32);
            let output = redact_secret(&input, Some(secret));
            assert!(output.len() <= input.len(), "secret: {secret:?}");
            assert!(
                output.chars().count() <= input.chars().count(),
                "secret: {secret:?}"
            );
        }
    }
}
