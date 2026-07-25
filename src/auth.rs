//! Bounded browser authentication for the ChatGPT Codex subscription API.
//!
//! This module deliberately owns only the OAuth and credential-store boundary. It does not
//! decide which provider a session uses.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::redaction::{conflicts_with_protected_literal, redaction_marker};

pub const DEFAULT_AUTH_ISSUER: &str = "https://auth.openai.com";
pub const DEFAULT_TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
pub const DEFAULT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CALLBACK_HOST: &str = "127.0.0.1";
pub const CALLBACK_REDIRECT_HOST: &str = "localhost";
pub const CALLBACK_PORT: u16 = 1455;
pub const CALLBACK_PATH: &str = "/auth/callback";
pub const REFRESH_WINDOW_SECONDS: i64 = 300;
const MAX_CALLBACK_REQUEST_BYTES: usize = 16 * 1024;
const MAX_TOKEN_RESPONSE_BYTES: usize = 256 * 1024;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthError(String);

impl AuthError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AuthError {}

impl From<io::Error> for AuthError {
    fn from(_: io::Error) -> Self {
        Self::new("authentication storage error")
    }
}

/// OAuth material persisted by Lucy. The JSON names are intentionally short because this file is
/// user-managed state, while aliases let a future migration read conventional token names.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodexCredentials {
    #[serde(rename = "access", alias = "access_token")]
    pub access: String,
    #[serde(rename = "refresh", alias = "refresh_token")]
    pub refresh: String,
    pub expires_at: Option<i64>,
    pub account_id: String,
}

impl CodexCredentials {
    pub fn near_expiry(&self, now: i64) -> bool {
        self.expires_at
            .is_some_and(|expires_at| expires_at <= now.saturating_add(REFRESH_WINDOW_SECONDS))
    }
}

/// Resolve Lucy's credential path without assuming that either XDG variable is set.
///
/// Data storage wins when both XDG locations are available. The config location is retained as a
/// fallback so installations that deliberately keep all Lucy state under XDG_CONFIG_HOME remain
/// supported.
pub fn credential_path(home: &Path) -> PathBuf {
    credential_path_from_xdg(
        home,
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
    )
}

pub fn credential_path_from_xdg(
    home: &Path,
    xdg_data_home: Option<&std::ffi::OsStr>,
    xdg_config_home: Option<&std::ffi::OsStr>,
) -> PathBuf {
    let root = xdg_data_home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            xdg_config_home
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        })
        .unwrap_or_else(|| home.join(".config"));
    root.join("lucy").join("codex-credentials.json")
}

fn validate_credentials(credentials: &CodexCredentials) -> Result<(), AuthError> {
    if credentials.access.is_empty()
        || credentials.refresh.is_empty()
        || credentials.account_id.is_empty()
    {
        return Err(AuthError::new("credentials are incomplete"));
    }
    for token in [&credentials.access, &credentials.refresh] {
        if conflicts_with_protected_literal(token) || redaction_marker(token).is_none() {
            return Err(AuthError::new("credentials cannot be safely stored"));
        }
    }
    Ok(())
}

/// A private, symlink-safe JSON credential store.
#[derive(Debug, Clone)]
pub struct AuthStore {
    path: PathBuf,
}

impl AuthStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn for_home(home: &Path) -> Self {
        Self::new(credential_path(home))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<CodexCredentials>, AuthError> {
        reject_symlink(&self.path).map_err(|_| AuthError::new("unable to secure credentials"))?;
        let mut file = match OpenOptions::new().read(true).open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(AuthError::new("unable to read credentials")),
        };
        ensure_mode(&self.path).map_err(|_| AuthError::new("unable to secure credentials"))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|_| AuthError::new("unable to read credentials"))?;
        if bytes.len() > MAX_TOKEN_RESPONSE_BYTES {
            return Err(AuthError::new("credentials exceeded the storage limit"));
        }
        let credentials: CodexCredentials = serde_json::from_slice(&bytes)
            .map_err(|_| AuthError::new("credentials are invalid"))?;
        validate_credentials(&credentials)?;
        Ok(Some(credentials))
    }

    pub fn save(&self, credentials: &CodexCredentials) -> Result<(), AuthError> {
        validate_credentials(credentials)?;
        let directory = self
            .path
            .parent()
            .ok_or_else(|| AuthError::new("unable to secure credentials"))?;
        ensure_private_directory(directory)?;
        reject_symlink(&self.path).map_err(|_| AuthError::new("unable to secure credentials"))?;

        let bytes = serde_json::to_vec_pretty(credentials)
            .map_err(|_| AuthError::new("unable to encode credentials"))?;
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = directory.join(format!(
            ".{}.{}.tmp",
            self.path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("credentials"),
            counter
        ));
        reject_symlink(&temporary).map_err(|_| AuthError::new("unable to secure credentials"))?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let result = (|| {
            let mut file = options
                .open(&temporary)
                .map_err(|_| AuthError::new("unable to write credentials"))?;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| AuthError::new("unable to write credentials"))?;
            ensure_mode(&temporary).map_err(|_| AuthError::new("unable to secure credentials"))?;
            fs::rename(&temporary, &self.path)
                .map_err(|_| AuthError::new("unable to replace credentials"))?;
            ensure_mode(&self.path).map_err(|_| AuthError::new("unable to secure credentials"))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn logout(&self) -> Result<bool, AuthError> {
        reject_symlink(&self.path).map_err(|_| AuthError::new("unable to secure credentials"))?;
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(AuthError::new("unable to remove credentials")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkceChallenge {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce() -> Result<PkceChallenge, AuthError> {
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|_| AuthError::new("unable to initialize OAuth"))?;
    let verifier = URL_SAFE_NO_PAD.encode(random);
    let digest = Sha256::digest(verifier.as_bytes());
    Ok(PkceChallenge {
        verifier,
        challenge: URL_SAFE_NO_PAD.encode(digest),
    })
}

#[derive(Debug, Clone)]
pub struct OAuthEndpoints {
    pub authorize: String,
    pub token: String,
    pub client_id: String,
    pub issuer: String,
}

impl Default for OAuthEndpoints {
    fn default() -> Self {
        Self {
            authorize: format!("{DEFAULT_AUTH_ISSUER}/oauth/authorize"),
            token: DEFAULT_TOKEN_ENDPOINT.to_owned(),
            client_id: DEFAULT_CLIENT_ID.to_owned(),
            issuer: DEFAULT_AUTH_ISSUER.to_owned(),
        }
    }
}

/// Perform the browser authorization-code flow and persist the returned credentials.
pub fn login(home: &Path) -> Result<CodexCredentials, AuthError> {
    login_with_endpoints(home, &OAuthEndpoints::default())
}

pub fn login_with_endpoints(
    home: &Path,
    endpoints: &OAuthEndpoints,
) -> Result<CodexCredentials, AuthError> {
    let pkce = generate_pkce()?;
    let state = random_url_value()?;
    let listener = TcpListener::bind((CALLBACK_HOST, CALLBACK_PORT))
        .map_err(|_| AuthError::new("unable to bind OAuth callback on 127.0.0.1:1455"))?;
    let redirect_uri = format!("http://{CALLBACK_REDIRECT_HOST}:{CALLBACK_PORT}{CALLBACK_PATH}");
    let authorize_url = build_authorize_url(endpoints, &redirect_uri, &pkce, &state)?;
    if !open_browser(&authorize_url) {
        eprintln!("Open this URL in your browser to sign in with Codex:\n{authorize_url}");
    }

    let (code, callback_error) = receive_callback(&listener, &state)?;
    if let Some(error) = callback_error {
        return Err(error);
    }
    let code = code.ok_or_else(|| AuthError::new("OAuth callback did not contain a code"))?;
    let credentials = exchange_code(endpoints, &redirect_uri, &pkce.verifier, &code)?;
    AuthStore::for_home(home).save(&credentials)?;
    Ok(credentials)
}

fn build_authorize_url(
    endpoints: &OAuthEndpoints,
    redirect_uri: &str,
    pkce: &PkceChallenge,
    state: &str,
) -> Result<String, AuthError> {
    let mut url = reqwest::Url::parse(&endpoints.authorize)
        .map_err(|_| AuthError::new("invalid OAuth authorize endpoint"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &endpoints.client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair(
            "scope",
            "openid profile email offline_access api.connectors.read api.connectors.invoke",
        )
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", "lucy");
    Ok(url.to_string())
}

fn receive_callback(
    listener: &TcpListener,
    expected_state: &str,
) -> Result<(Option<String>, Option<AuthError>), AuthError> {
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(_) => return Err(AuthError::new("OAuth callback server failed")),
        };
        let request = read_http_request(&mut stream)?;
        let target = request
            .strip_prefix("GET ")
            .and_then(|request| request.split_whitespace().next())
            .ok_or_else(|| AuthError::new("OAuth callback request was invalid"))?;
        let url = reqwest::Url::parse(&format!("http://localhost{target}"))
            .map_err(|_| AuthError::new("OAuth callback request was invalid"))?;
        if url.path() != CALLBACK_PATH {
            write_callback(&mut stream, 404, "Not found")?;
            continue;
        }
        let query: std::collections::HashMap<String, String> =
            url.query_pairs().into_owned().collect();
        let state_valid = query.get("state").map(String::as_str) == Some(expected_state);
        if !state_valid {
            write_callback(&mut stream, 400, "Authentication state was rejected.")?;
            continue;
        }
        if query.contains_key("error") {
            write_callback(&mut stream, 400, "Authentication was not completed.")?;
            return Ok((None, Some(AuthError::new("OAuth authorization was denied"))));
        }
        let code = query
            .get("code")
            .filter(|code| !code.is_empty())
            .cloned()
            .ok_or_else(|| AuthError::new("OAuth callback did not contain a code"))?;
        write_callback(
            &mut stream,
            200,
            "Authentication complete. You may close this window.",
        )?;
        return Ok((Some(code), None));
    }
    Err(AuthError::new("OAuth callback server stopped"))
}

fn read_http_request(stream: &mut TcpStream) -> Result<String, AuthError> {
    stream
        .set_read_timeout(Some(Duration::from_secs(120)))
        .map_err(|_| AuthError::new("OAuth callback server failed"))?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = stream
            .read(&mut chunk)
            .map_err(|_| AuthError::new("OAuth callback request could not be read"))?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() > MAX_CALLBACK_REQUEST_BYTES {
            return Err(AuthError::new("OAuth callback request was too large"));
        }
    }
    String::from_utf8(bytes).map_err(|_| AuthError::new("OAuth callback request was invalid"))
}

fn write_callback(stream: &mut TcpStream, status: u16, body: &str) -> Result<(), AuthError> {
    let response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .map_err(|_| AuthError::new("OAuth callback response failed"))
}

fn exchange_code(
    endpoints: &OAuthEndpoints,
    redirect_uri: &str,
    verifier: &str,
    code: &str,
) -> Result<CodexCredentials, AuthError> {
    let response = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| AuthError::new("unable to initialize OAuth HTTP client"))?
        .post(&endpoints.token)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", endpoints.client_id.as_str()),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ])
        .send()
        .map_err(|_| AuthError::new("OAuth token exchange failed"))?;
    parse_token_response(response)
}

fn parse_token_response(
    response: reqwest::blocking::Response,
) -> Result<CodexCredentials, AuthError> {
    if !response.status().is_success() {
        return Err(AuthError::new("OAuth token exchange failed"));
    }
    let mut bytes = Vec::new();
    response
        .take((MAX_TOKEN_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| AuthError::new("OAuth token response could not be read"))?;
    if bytes.len() > MAX_TOKEN_RESPONSE_BYTES {
        return Err(AuthError::new(
            "OAuth token response exceeded the response limit",
        ));
    }
    let payload: TokenResponse = serde_json::from_slice(&bytes)
        .map_err(|_| AuthError::new("OAuth token response was invalid"))?;
    let access = non_empty(payload.access_token)
        .ok_or_else(|| AuthError::new("OAuth token response was incomplete"))?;
    let refresh = non_empty(payload.refresh_token)
        .ok_or_else(|| AuthError::new("OAuth token response was incomplete"))?;
    let account_id = payload
        .account_id
        .or(payload.chatgpt_account_id)
        .or_else(|| payload.id_token.as_deref().and_then(account_id_from_jwt))
        .and_then(|value| non_empty(Some(value)))
        .ok_or_else(|| AuthError::new("OAuth token response did not contain an account"))?;
    let expires_at = payload
        .expires_in
        .map(|seconds| now_seconds().saturating_add(seconds));
    Ok(CodexCredentials {
        access,
        refresh,
        expires_at,
        account_id,
    })
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    account_id: Option<String>,
    chatgpt_account_id: Option<String>,
    id_token: Option<String>,
}

fn account_id_from_jwt(jwt: &str) -> Option<String> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            value
                .get("chatgpt_account_id")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            value
                .get("organizations")
                .and_then(serde_json::Value::as_array)
                .and_then(|organizations| organizations.first())
                .and_then(|organization| organization.get("id"))
                .and_then(serde_json::Value::as_str)
        })
        .map(str::to_owned)
}

/// Refresh a credential set, retaining a rotated refresh token when the authority returns one.
pub fn refresh_credentials(
    credentials: &CodexCredentials,
    token_endpoint: &str,
    client_id: &str,
) -> Result<CodexCredentials, AuthError> {
    let response = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| AuthError::new("unable to initialize OAuth HTTP client"))?
        .post(token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", credentials.refresh.as_str()),
        ])
        .send()
        .map_err(|_| AuthError::new("OAuth token refresh failed"))?;
    if !response.status().is_success() {
        return Err(AuthError::new("OAuth token refresh failed"));
    }
    let mut bytes = Vec::new();
    response
        .take((MAX_TOKEN_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| AuthError::new("OAuth token response could not be read"))?;
    if bytes.len() > MAX_TOKEN_RESPONSE_BYTES {
        return Err(AuthError::new(
            "OAuth token response exceeded the response limit",
        ));
    }
    let payload: RefreshResponse = serde_json::from_slice(&bytes)
        .map_err(|_| AuthError::new("OAuth token response was invalid"))?;
    let access = non_empty(payload.access_token)
        .ok_or_else(|| AuthError::new("OAuth token response was incomplete"))?;
    Ok(CodexCredentials {
        access,
        refresh: non_empty(payload.refresh_token).unwrap_or_else(|| credentials.refresh.clone()),
        expires_at: payload
            .expires_in
            .map(|seconds| now_seconds().saturating_add(seconds))
            .or(credentials.expires_at),
        account_id: non_empty(payload.account_id).unwrap_or_else(|| credentials.account_id.clone()),
    })
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    account_id: Option<String>,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

fn random_url_value() -> Result<String, AuthError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| AuthError::new("unable to initialize OAuth"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let command = ("open", vec![url]);
    #[cfg(target_os = "linux")]
    let command = ("xdg-open", vec![url]);
    #[cfg(target_os = "windows")]
    let command = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let command: (&str, Vec<&str>) = ("", Vec::new());

    !command.0.is_empty() && Command::new(command.0).args(command.1).spawn().is_ok()
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn reject_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::other("symlink")),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), AuthError> {
    ensure_directory(path).map_err(|_| AuthError::new("unable to secure credentials directory"))?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| AuthError::new("unable to secure credentials directory"))?;
    Ok(())
}

fn ensure_directory(path: &Path) -> io::Result<()> {
    reject_symlink(path)?;
    if !path.exists() {
        fs::create_dir_all(path)?;
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::other("not a directory"));
    }
    Ok(())
}

fn ensure_mode(path: &Path) -> io::Result<()> {
    reject_symlink(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::other("not a file"));
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn pkce_uses_s256_without_padding() {
        let pkce = generate_pkce().expect("pkce");
        assert!((43..=128).contains(&pkce.verifier.len()));
        assert!(!pkce.challenge.contains('='));
        let digest = Sha256::digest(pkce.verifier.as_bytes());
        assert_eq!(pkce.challenge, URL_SAFE_NO_PAD.encode(digest));
    }

    #[test]
    fn authorize_url_matches_the_codex_loopback_contract() {
        let pkce = PkceChallenge {
            verifier: "verifier".to_owned(),
            challenge: "challenge".to_owned(),
        };
        let url = build_authorize_url(
            &OAuthEndpoints::default(),
            "http://localhost:1455/auth/callback",
            &pkce,
            "state",
        )
        .expect("authorize URL");
        let parsed = reqwest::Url::parse(&url).expect("URL");
        assert_eq!(
            parsed
                .query_pairs()
                .find(|(key, _)| key == "redirect_uri")
                .map(|(_, value)| value.into_owned()),
            Some("http://localhost:1455/auth/callback".to_owned())
        );
        assert_eq!(
            parsed
                .query_pairs()
                .find(|(key, _)| key == "originator")
                .map(|(_, value)| value.into_owned()),
            Some("lucy".to_owned())
        );
    }

    #[test]
    fn credential_path_prefers_data_then_config_and_rejects_relative_xdg() {
        assert_eq!(
            credential_path_from_xdg(
                Path::new("/home/test"),
                Some(OsStr::new("/tmp/data")),
                Some(OsStr::new("/tmp/config"))
            ),
            PathBuf::from("/tmp/data/lucy/codex-credentials.json")
        );
        assert_eq!(
            credential_path_from_xdg(Path::new("/home/test"), None, Some(OsStr::new("relative"))),
            PathBuf::from("/home/test/.config/lucy/codex-credentials.json")
        );
    }

    #[test]
    fn store_is_private_and_round_trips_without_secret_in_error() {
        let directory = std::env::temp_dir().join(format!(
            "lucy-auth-{}",
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let path = directory.join("credentials.json");
        let store = AuthStore::new(path.clone());
        let credentials = CodexCredentials {
            access: "access-secret".to_owned(),
            refresh: "refresh-secret".to_owned(),
            expires_at: Some(10),
            account_id: "account".to_owned(),
        };
        store.save(&credentials).expect("save");
        assert_eq!(store.load().expect("load"), Some(credentials));
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
        store.logout().expect("logout");
        assert_eq!(store.load().expect("missing"), None);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn refresh_keeps_rotated_tokens_and_account_metadata() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        let address = listener.local_addr().expect("address");
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let body = r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600,"account_id":"account-2"}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            )
            .expect("response");
        });
        let credentials = CodexCredentials {
            access: "old-access".to_owned(),
            refresh: "old-refresh".to_owned(),
            expires_at: Some(1),
            account_id: "account-1".to_owned(),
        };
        let refreshed = refresh_credentials(&credentials, &format!("http://{address}"), "client")
            .expect("refresh");
        thread.join().expect("server");
        assert_eq!(refreshed.access, "new-access");
        assert_eq!(refreshed.refresh, "new-refresh");
        assert_eq!(refreshed.account_id, "account-2");
        assert!(refreshed.expires_at.unwrap_or_default() > credentials.expires_at.unwrap());
    }

    #[test]
    fn store_rejects_unsafe_access_or_refresh_tokens_before_writing() {
        let directory = std::env::temp_dir().join(format!(
            "lucy-auth-unsafe-{}",
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let path = directory.join("credentials.json");
        let store = AuthStore::new(path.clone());
        for (access, refresh) in [("123", "refresh"), ("access", "refresh\"token")] {
            let credentials = CodexCredentials {
                access: access.to_owned(),
                refresh: refresh.to_owned(),
                expires_at: Some(10),
                account_id: "account".to_owned(),
            };
            assert!(store.save(&credentials).is_err());
            assert!(!path.exists());
        }
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn expiry_window_is_five_minutes() {
        let credentials = CodexCredentials {
            access: "a".to_owned(),
            refresh: "r".to_owned(),
            expires_at: Some(1_000),
            account_id: "id".to_owned(),
        };
        assert!(credentials.near_expiry(700));
        assert!(!credentials.near_expiry(699));
    }
}
