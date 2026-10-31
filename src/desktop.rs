use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::cancellation::CancellationToken;

pub(crate) const DESCRIPTION: &str = "Operate desktop apps using bundled agent-desktop. args is an argv array, not a shell command. Before first use call [\"skills\",\"get\",\"desktop\"] and [\"skills\",\"get\",\"platform\"]. Observe with snapshot, act on snapshot refs, then snapshot to verify. Use [command,\"--help\"] for flags. Headless by default; --headed permits physical input. OS permissions are required. Desktop content is sent to the model and saved in the session. Results contain stdout/stderr; screenshot files are not image attachments.";
const ARCHIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/agent-desktop.tar.gz"));

pub(crate) fn parameters() -> Value {
    json!({"type":"object","properties":{"args":{"type":"array","items":{"type":"string"},"minItems":1}},"required":["args"],"additionalProperties":false})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    args: Vec<String>,
}

pub(crate) fn execute(
    arguments: &str,
    home: &Path,
    cwd: &Path,
    api_key_env: &str,
    secret: Option<&str>,
    cancellation: Option<&CancellationToken>,
) -> Value {
    let error = |message: String| json!({"error": crate::command::redact_secret(&message, secret)});
    let arguments: Arguments = match serde_json::from_str(arguments) {
        Ok(arguments) => arguments,
        Err(err) => return error(format!("invalid desktop arguments: {err}")),
    };
    if arguments.args.is_empty() || arguments.args.iter().any(|arg| arg.contains('\0')) {
        return error("desktop args must be a nonempty array of strings without NUL bytes".into());
    }
    let label = format!("agent-desktop {}", json!(arguments.args));
    if cancellation.is_some_and(|token| token.is_cancelled()) {
        return json!(crate::command::canceled_result(
            &json!({"command":label}).to_string(),
            secret.unwrap_or_default()
        ));
    }
    let program = match materialize(home) {
        Ok(program) => program,
        Err(err) => return error(format!("unable to install bundled agent-desktop: {err}")),
    };
    json!(crate::command::execute_program(
        &program,
        &arguments.args,
        &label,
        cwd,
        api_key_env,
        secret,
        cancellation,
    ))
}

// A stable, versioned path preserves macOS permission identity. Publish each file
// atomically so concurrent Lucy processes never execute a partially written binary.
fn materialize(home: &Path) -> io::Result<PathBuf> {
    let directory = crate::config::lucy_dir(home)
        .join("desktop")
        .join(concat!("1.0.2-", env!("LUCY_DESKTOP_TARGET")));
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(ARCHIVE));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.into_owned();
        if !matches!(
            name.to_str(),
            Some(
                "agent-desktop"
                    | "agent-desktop-macos-helper"
                    | "agent-desktop-linux-clipboard-owner"
            )
        ) || !entry.header().entry_type().is_file()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected bundled archive entry",
            ));
        }
        let destination = directory.join(name);
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        if fs::symlink_metadata(&destination)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o777 == 0o700)
            && fs::read(&destination).is_ok_and(|existing| existing == bytes)
        {
            continue;
        }
        let mut file = tempfile::NamedTempFile::new_in(&directory)?;
        file.write_all(&bytes)?;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o700))?;
        file.persist(&destination).map_err(|err| err.error)?;
    }
    let mut license = tempfile::NamedTempFile::new_in(&directory)?;
    license.write_all(include_bytes!("../third-party/agent-desktop/LICENSE"))?;
    license
        .persist(directory.join("LICENSE"))
        .map_err(|err| err.error)?;
    Ok(directory.join("agent-desktop"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_binary_and_helpers_are_installed_and_execute_offline() {
        let home = tempfile::tempdir().unwrap();
        let binary = materialize(home.path()).unwrap();
        assert!(binary.is_file());
        #[cfg(target_os = "macos")]
        assert!(binary
            .with_file_name("agent-desktop-macos-helper")
            .is_file());
        #[cfg(target_os = "linux")]
        assert!(binary
            .with_file_name("agent-desktop-linux-clipboard-owner")
            .is_file());
        assert_eq!(materialize(home.path()).unwrap(), binary);
        let result = execute(
            r#"{"args":["version"]}"#,
            home.path(),
            home.path(),
            "LUCY_TEST_SECRET",
            None,
            None,
        );
        assert_eq!(result["exit_code"], 0, "{result}");
        let output: Value = serde_json::from_str(result["stdout"].as_str().unwrap()).unwrap();
        assert_eq!(output["data"]["version"], "1.0.2");
    }

    #[test]
    fn bundled_skills_are_available_and_install_failures_are_reported() {
        let home = tempfile::tempdir().unwrap();
        let result = execute(
            r#"{"args":["skills","get","desktop"]}"#,
            home.path(),
            home.path(),
            "KEY",
            None,
            None,
        );
        assert_eq!(result["exit_code"], 0);
        let output: Value = serde_json::from_str(result["stdout"].as_str().unwrap()).unwrap();
        assert!(output["data"]["content"]
            .as_str()
            .unwrap()
            .contains("snapshot"));
        let blocked = home.path().join("blocked");
        fs::write(&blocked, "not a directory").unwrap();
        let result = execute(
            r#"{"args":["version"]}"#,
            &blocked,
            home.path(),
            "KEY",
            None,
            None,
        );
        assert!(result["error"]
            .as_str()
            .unwrap()
            .contains("unable to install"));
    }

    #[test]
    fn arguments_are_not_interpreted_by_a_shell_and_errors_are_returned() {
        let home = tempfile::tempdir().unwrap();
        let result = execute(
            r#"{"args":["no-such-command; touch injected"]}"#,
            home.path(),
            home.path(),
            "KEY",
            None,
            None,
        );
        assert_ne!(result["exit_code"], 0, "{result}");
        assert!(!home.path().join("injected").exists());
    }

    #[test]
    fn rejects_invalid_arguments_without_installing_or_running() {
        let home = tempfile::tempdir().unwrap();
        for args in [
            r#"{"args":[]}"#,
            r#"{"args":"version"}"#,
            r#"{"args":[1]}"#,
            r#"{"args":["version"],"background":true}"#,
            r#"{"args":["\u0000"]}"#,
        ] {
            assert!(execute(args, home.path(), home.path(), "KEY", None, None)
                .get("error")
                .is_some());
        }
        assert!(!crate::config::lucy_dir(home.path()).exists());
    }

    #[test]
    fn canceled_desktop_does_not_install_or_run() {
        let home = tempfile::tempdir().unwrap();
        let token = CancellationToken::default();
        token.cancel();
        let result = execute(
            r#"{"args":["version"]}"#,
            home.path(),
            home.path(),
            "KEY",
            None,
            Some(&token),
        );
        assert_eq!(result["canceled"], true);
        assert!(!crate::config::lucy_dir(home.path()).exists());
    }
}
