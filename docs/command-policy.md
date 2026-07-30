# Command deny policy

Lucy command execution is default-allow. If no policy hook is successfully loaded, model-selected commands proceed to the normal [`cmd` execution boundary](trust-model.md): they run with the invoking user's operating-system privileges and Lucy does not ask for approval.

## Configuration

Set `execution.policy` in `config.toml` to the policy executable:

```toml
[execution]
policy = "~/.config/lucy/deny-policy.sh"
```

A relative path is resolved under Lucy's configuration directory. A configured path must exist, must not itself be a symlink, and on Unix must not be group- or world-writable. Lucy then executes the path directly; the file therefore needs an executable format and permissions supported by the operating system.

Current limitation: `lucy doctor` does not inspect the configured policy. Lucy's current session startup also treats a policy path-resolution or validation error as if no hook was loaded. Check the path and permissions independently before relying on the hook. Once a valid hook is loaded, a hook spawn failure, timeout, unexpected exit, or wait error denies that command.

## Hook protocol

Before spawning each foreground or background model command, Lucy starts the hook and writes one JSON object to its stdin:

```json
{"version":1,"session_id":"session-id","cwd":"/workspace","command":"git status","background":false}
```

The hook receives:

- `version`: protocol version `1`;
- `session_id`: the active Lucy session ID;
- `cwd`: the session's fixed starting directory;
- `command`: the exact model-selected shell text;
- `background`: whether the command requested background execution.

The hook's active API-key environment variable is removed before spawn, but the rest of Lucy's environment is inherited. This removes one direct inheritance path; it is not credential or process isolation.

Exit status controls the result:

- `0`: allow the command;
- `10`: deny it and use up to 512 bytes of hook stdout as the reason;
- any other status, signal, spawn/wait failure, or the 5-second hook timeout: report a policy error and do not spawn that model command.

Policy stderr is not used as the denial reason. Policy evaluation runs before the model command process is created.

## Security boundary

The hook is a user-owned guardrail, not a sandbox or semantic proof of a command's effects. Alternate commands, interpreters, scripts, aliases, shell syntax, absolute paths, or indirect programs may reach an equivalent result. An allowed command runs as the invoking user and may modify an owner-writable hook or one of its dependencies before a later command is checked.

Repository instructions, skills, and model output cannot directly replace Lucy's already resolved policy path. They can still influence proposed command text, and an allowed user-privileged command can change files available to later policy evaluations or change configuration for a later Lucy process.

Use external filesystem, network, credential, container, VM, sandbox, or OS-account controls when the repository or input is untrusted. See the full [trust model](trust-model.md).
