use std::collections::BTreeMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;

const TOKEN_ENV: &str = "TELEGRAM_BOT_TOKEN";
const API_BASE: &str = "https://api.telegram.org";
const LONG_POLL_SECONDS: u64 = 30;
const REQUEST_TIMEOUT_SECONDS: u64 = 45;
const TYPING_REFRESH_SECONDS: u64 = 4;
const CHAT_ACTION_TIMEOUT_SECONDS: u64 = 2;
const TELEGRAM_TEXT_LIMIT: usize = 4096;

#[derive(Debug, Default, Deserialize, Serialize)]
struct GatewayState {
    #[serde(default)]
    next_update_id: Option<i64>,
    #[serde(default)]
    chats: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct UpdatesResponse {
    ok: bool,
    #[serde(default)]
    result: Vec<TelegramUpdate>,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    chat: TelegramChat,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct BasicResponse {
    ok: bool,
}

#[derive(Debug, Deserialize)]
struct ExecEnvelope {
    status: String,
    session_id: String,
    text: String,
}

pub fn run() -> Result<(), String> {
    let token = std::env::var(TOKEN_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{TOKEN_ENV} is not set"))?;

    // The gateway needs the token, model-executed child processes do not. Remove
    // it before Lucy starts any turn so cmd cannot inherit the transport secret.
    std::env::remove_var(TOKEN_ENV);

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set; Lucy needs a user home directory".to_owned())?;
    let cwd = std::env::current_dir().map_err(|_| "unable to resolve cwd".to_owned())?;
    let state_path = gateway_state_path(&home);
    let mut state = load_state(&state_path)?;
    let client = Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECONDS))
        .build()
        .map_err(|_| "unable to initialize Telegram client".to_owned())?;

    loop {
        let updates = get_updates(&client, &token, state.next_update_id)?;
        for update in updates {
            let next_update_id = update.update_id.saturating_add(1);
            let mut reply = None;
            if let Some(message) = update.message {
                if let Some(text) = message.text.filter(|text| !text.trim().is_empty()) {
                    let chat_key = message.chat.id.to_string();
                    let previous_session = state.chats.get(&chat_key).map(String::as_str);
                    let result = run_with_typing(&client, &token, message.chat.id, || {
                        run_lucy_turn(&home, &cwd, previous_session, &text)
                    });
                    match result {
                        Ok(result) => {
                            state.chats.insert(chat_key, result.session_id);
                            reply = Some((message.chat.id, result.text));
                        }
                        Err(_) => {
                            reply = Some((
                                message.chat.id,
                                "Lucy could not complete that turn.".to_owned(),
                            ));
                        }
                    }
                }
            }

            // A completed model turn may have side effects. Advance the Telegram
            // cursor before attempting delivery so a send failure cannot cause the
            // same update to execute twice after restart.
            state.next_update_id = Some(next_update_id);
            save_state(&state_path, &state)?;

            if let Some((chat_id, text)) = reply {
                send_message(&client, &token, chat_id, &text)?;
            }
        }
    }
}

fn gateway_state_path(home: &Path) -> PathBuf {
    home.join(".lucy").join("gateways").join("telegram.json")
}

fn load_state(path: &Path) -> Result<GatewayState, String> {
    match fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|_| "invalid Telegram gateway state".to_owned())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(GatewayState::default()),
        Err(_) => Err("unable to read Telegram gateway state".to_owned()),
    }
}

fn save_state(path: &Path, state: &GatewayState) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "invalid Telegram gateway state path".to_owned())?;
    fs::create_dir_all(parent).map_err(|_| "unable to create Telegram gateway state".to_owned())?;
    set_private_directory_permissions(parent)?;

    let payload = serde_json::to_vec_pretty(state)
        .map_err(|_| "unable to encode Telegram gateway state".to_owned())?;
    let tmp = parent.join(format!(".telegram.json.tmp-{}", std::process::id()));
    fs::write(&tmp, payload).map_err(|_| "unable to write Telegram gateway state".to_owned())?;
    set_private_file_permissions(&tmp)?;
    fs::rename(&tmp, path).map_err(|_| "unable to commit Telegram gateway state".to_owned())?;
    set_private_file_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| "unable to protect Telegram gateway state directory".to_owned())
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| "unable to protect Telegram gateway state".to_owned())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn get_updates(
    client: &Client,
    token: &str,
    next_update_id: Option<i64>,
) -> Result<Vec<TelegramUpdate>, String> {
    let response = client
        .post(telegram_method_url(token, "getUpdates"))
        .json(&json!({
            "offset": next_update_id.unwrap_or(0),
            "timeout": LONG_POLL_SECONDS,
            "allowed_updates": ["message"],
        }))
        .send()
        .map_err(|_| "Telegram getUpdates request failed".to_owned())?;
    let status = response.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            401 => "Telegram rejected TELEGRAM_BOT_TOKEN".to_owned(),
            409 => "Telegram bot is already being polled by another gateway".to_owned(),
            _ => "Telegram getUpdates request failed".to_owned(),
        });
    }
    let payload = response
        .json::<UpdatesResponse>()
        .map_err(|_| "invalid Telegram getUpdates response".to_owned())?;
    if !payload.ok {
        return Err("Telegram getUpdates request failed".to_owned());
    }
    Ok(payload.result)
}

fn send_chat_action(client: &Client, token: &str, chat_id: i64) -> Result<(), String> {
    let response = client
        .post(telegram_method_url(token, "sendChatAction"))
        .json(&json!({
            "chat_id": chat_id,
            "action": "typing",
        }))
        .timeout(Duration::from_secs(CHAT_ACTION_TIMEOUT_SECONDS))
        .send()
        .map_err(|_| "Telegram sendChatAction request failed".to_owned())?;
    if !response.status().is_success() {
        return Err("Telegram sendChatAction request failed".to_owned());
    }
    let payload = response
        .json::<BasicResponse>()
        .map_err(|_| "invalid Telegram sendChatAction response".to_owned())?;
    if !payload.ok {
        return Err("Telegram sendChatAction request failed".to_owned());
    }
    Ok(())
}

fn run_with_typing<T, F>(client: &Client, token: &str, chat_id: i64, turn: F) -> Result<T, String>
where
    T: Send,
    F: FnOnce() -> Result<T, String> + Send,
{
    std::thread::scope(|scope| {
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let handle = scope.spawn(move || {
            let _ = result_tx.send(turn());
        });
        let result = wait_with_periodic_action(
            &result_rx,
            Duration::from_secs(TYPING_REFRESH_SECONDS),
            || send_chat_action(client, token, chat_id),
        );
        handle.join().map_err(|_| "Lucy turn failed".to_owned())?;
        result
    })
}

fn wait_with_periodic_action<T, F>(
    result_rx: &mpsc::Receiver<Result<T, String>>,
    interval: Duration,
    mut action: F,
) -> Result<T, String>
where
    F: FnMut() -> Result<(), String>,
{
    loop {
        // Chat actions are ephemeral transport hints. Failure must not change the
        // model turn result or suppress the eventual reply.
        let refresh_started = Instant::now();
        let _ = action();
        let wait = interval.saturating_sub(refresh_started.elapsed());
        match result_rx.recv_timeout(wait) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("Lucy turn failed".to_owned());
            }
        }
    }
}

fn send_message(client: &Client, token: &str, chat_id: i64, text: &str) -> Result<(), String> {
    let text = if text.is_empty() {
        "(empty response)"
    } else {
        text
    };
    for chunk in split_telegram_text(text) {
        let response = client
            .post(telegram_method_url(token, "sendMessage"))
            .json(&json!({
                "chat_id": chat_id,
                "text": chunk,
            }))
            .send()
            .map_err(|_| "Telegram sendMessage request failed".to_owned())?;
        if !response.status().is_success() {
            return Err("Telegram sendMessage request failed".to_owned());
        }
        let payload = response
            .json::<BasicResponse>()
            .map_err(|_| "invalid Telegram sendMessage response".to_owned())?;
        if !payload.ok {
            return Err("Telegram sendMessage request failed".to_owned());
        }
    }
    Ok(())
}

fn run_lucy_turn(
    home: &Path,
    cwd: &Path,
    session_id: Option<&str>,
    prompt: &str,
) -> Result<ExecEnvelope, String> {
    let mut args = vec!["exec".to_owned()];
    if let Some(session_id) = session_id {
        args.push("--session".to_owned());
        args.push(session_id.to_owned());
    }
    args.extend(["--output".to_owned(), "json".to_owned(), "-".to_owned()]);

    let mut output = Vec::new();
    let mut diagnostics = Vec::new();
    let exit = crate::run_cli_at_home(
        &args,
        Cursor::new(prompt.as_bytes().to_vec()),
        &mut output,
        &mut diagnostics,
        home,
        cwd,
    );
    if exit != 0 {
        return Err("Lucy turn failed".to_owned());
    }
    let result = serde_json::from_slice::<ExecEnvelope>(&output)
        .map_err(|_| "invalid Lucy exec response".to_owned())?;
    if result.status != "completed" || result.session_id.is_empty() {
        return Err("Lucy turn did not complete".to_owned());
    }
    Ok(result)
}

fn telegram_method_url(token: &str, method: &str) -> String {
    format!("{API_BASE}/bot{token}/{method}")
}

fn split_telegram_text(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0;
    for ch in text.chars() {
        if current_len == TELEGRAM_TEXT_LIMIT {
            chunks.push(std::mem::take(&mut current));
            current_len = 0;
        }
        current.push(ch);
        current_len += 1;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_text_split_preserves_unicode_and_limit() {
        let input = "가".repeat(TELEGRAM_TEXT_LIMIT + 3);
        let chunks = split_telegram_text(&input);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), TELEGRAM_TEXT_LIMIT);
        assert_eq!(chunks[1].chars().count(), 3);
        assert_eq!(chunks.concat(), input);
    }

    #[test]
    fn gateway_state_defaults_for_new_fields() {
        let state: GatewayState = serde_json::from_str("{}").expect("state");
        assert_eq!(state.next_update_id, None);
        assert!(state.chats.is_empty());
    }

    #[test]
    fn telegram_method_url_has_expected_shape() {
        assert_eq!(
            telegram_method_url("test-token", "getUpdates"),
            "https://api.telegram.org/bottest-token/getUpdates"
        );
    }

    #[test]
    fn periodic_action_runs_immediately_and_refreshes_until_result() {
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let mut calls = 0;

        let result = wait_with_periodic_action(
            &result_rx,
            Duration::from_millis(1),
            || -> Result<(), String> {
                calls += 1;
                if calls == 2 {
                    result_tx
                        .send(Ok("completed"))
                        .expect("result receiver remains open");
                }
                Err("typing is best effort".to_owned())
            },
        )
        .expect("typing failure must not fail the turn");

        assert_eq!(result, "completed");
        assert_eq!(calls, 2);
    }
}
