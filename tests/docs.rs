use std::path::Path;

fn assert_relative_markdown_links_resolve(source_path: &str, source: &str) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let parent = root
        .join(source_path)
        .parent()
        .expect("documentation path has a parent")
        .to_path_buf();
    let mut remaining = source;
    while let Some(open) = remaining.find("](") {
        remaining = &remaining[open + 2..];
        let Some(close) = remaining.find(')') else {
            panic!("unterminated Markdown link in {source_path}");
        };
        let target = &remaining[..close];
        remaining = &remaining[close + 1..];
        if target.is_empty()
            || target.starts_with('#')
            || target.contains("://")
            || target.starts_with("mailto:")
        {
            continue;
        }
        let path = target.split('#').next().unwrap_or(target);
        assert!(
            parent.join(path).exists(),
            "broken relative link in {source_path}: {target}"
        );
    }
}

#[test]
fn trust_model_links_and_authority_wording_stay_consistent() {
    let trust = include_str!("../docs/trust-model.md");
    let readme = include_str!("../README.md");
    let homepage = include_str!("../site/index.html");
    let setup = include_str!("../src/setup.rs");
    let diagnostics = include_str!("../docs/diagnostics.md");
    let command_policy = include_str!("../docs/command-policy.md");

    for required in [
        "arbitrary shell command text",
        "not a security sandbox",
        "allowed by default",
        "removes the active API-provider credential variable",
        "External isolation",
    ] {
        assert!(
            trust.contains(required),
            "missing trust-model claim: {required}"
        );
    }
    assert!(readme.contains("[trust model](docs/trust-model.md)"));
    assert!(readme.contains("[command-policy contract](docs/command-policy.md)"));
    assert!(readme.contains("[diagnostics contract](docs/diagnostics.md)"));
    assert!(readme.contains("each foreground or background model `cmd` command"));
    assert!(readme.contains("not a sandbox"));
    assert!(!readme.contains("Safe local command execution"));
    assert!(homepage.contains("https://github.com/jwoo0122/lucy/blob/main/docs/trust-model.md"));
    assert!(homepage.contains("not a sandbox"));
    assert!(homepage.contains("external isolation"));
    assert!(setup.contains("https://github.com/jwoo0122/lucy/blob/main/docs/trust-model.md"));
    assert!(command_policy.contains("[trust model](trust-model.md)"));
    assert!(diagnostics.contains("[trust model](trust-model.md)"));
    assert!(diagnostics.contains("[command policy](command-policy.md)"));

    for (path, contents) in [
        ("README.md", readme),
        ("docs/trust-model.md", trust),
        ("docs/command-policy.md", command_policy),
        ("docs/diagnostics.md", diagnostics),
    ] {
        assert_relative_markdown_links_resolve(path, contents);
    }
}
#[test]
fn trust_model_key_claims_remain_backed_by_source() {
    let command = include_str!("../src/command.rs");
    let policy = include_str!("../src/policy.rs");
    let session = include_str!("../src/session.rs");
    let auth = include_str!("../src/auth.rs");
    let model = include_str!("../src/model.rs");
    let codex = include_str!("../src/codex_provider.rs");
    let app = include_str!("../src/app.rs");
    let doctor = include_str!("../src/doctor.rs");
    let config = include_str!("../src/config.rs");

    for implementation in [
        "pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10 * 60);",
        "pub const COMMAND_OUTPUT_CAP: usize = 64 * 1024;",
        ".arg(\"-lc\")",
        ".stdin(Stdio::null())",
        "process.env_remove(api_key_env);",
        "kill_process_group(child_id);",
    ] {
        assert!(
            command.contains(implementation),
            "missing command bound: {implementation}"
        );
    }
    assert!(policy.contains("const POLICY_TIMEOUT: Duration = Duration::from_secs(5);"));
    assert!(policy.contains("const POLICY_OUTPUT_CAP: usize = 512;"));
    assert!(policy.contains("return PolicyOutcome::Unconfigured;"));
    assert!(policy.contains("child.env_remove(api_key_env);"));
    assert!(config.contains("pub fn resolved_policy("));
    assert!(app.contains(".and_then(|config| config.resolved_policy(home).ok())"));
    assert!(!doctor.contains("resolved_policy"));
    assert!(session.contains("options.write(true).append(true);"));
    assert!(session.contains("home.join(\".lucy\").join(\"sessions\")"));
    assert!(auth.contains("join(\"codex-credentials.json\")"));
    assert!(auth.contains("std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);"));
    assert!(model.contains("pub const OBSERVATION_ROLE: &str = \"observation\";"));
    assert!(model.contains("let role = if self.role == OBSERVATION_ROLE"));
    assert!(codex.contains("fn codex_request_maps_observations_to_unprivileged_user_input()"));
}
