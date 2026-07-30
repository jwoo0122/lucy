# Lucy trust model

Lucy is a local coding-agent harness, not a security sandbox. This document describes the authority the current implementation gives the model, the boundaries Lucy enforces, and the controls that remain the user's responsibility.

## Model authority

Lucy authorizes the model to execute arbitrary shell command text with the operating-system privileges of the user who invoked Lucy. Without external OS isolation, a selected command can read files that user can read; modify or delete writable files; leave the starting repository; access the network; start processes; install or invoke programs; and use authentication mechanisms available to those programs.

The model-facing tool is `cmd`. Lucy trusts the model to choose command text. Repository content, instructions, skills, dependency documentation, command output, compiler and test output, HTTP responses, generated files, and output from other agents are untrusted inputs that can influence that choice.

## Shell and environment contract

Lucy runs each command through `$SHELL -lc`, falling back to `/bin/sh -lc` when `SHELL` is unset or empty. Commands are non-interactive, receive no PTY, and have closed stdin. They begin in the session's fixed starting directory. Shell startup behavior follows the selected shell's own login/non-interactive rules; Lucy does not promise that `.zshrc`, `.bashrc`, aliases, functions, or interactive plugins are loaded, and it does not parse or sanitize startup files.

The ordinary parent environment is inherited. Lucy removes the active API-provider credential variable from the command's direct child environment before spawn. This is prevention of one direct inheritance path, not credential isolation: shell startup files, credential helpers, commands, or other processes can independently recreate or retrieve credentials.

## Boundaries Lucy enforces

Current implemented bounds include:

- a fixed 10-minute command timeout (`COMMAND_TIMEOUT` in [`src/command.rs`](../src/command.rs));
- independent 64 KiB stdout and stderr capture caps (`COMMAND_OUTPUT_CAP`);
- closed command stdin, a stable session cwd, cancellation checks, and process-group cleanup with a bounded capture grace period; descendants that deliberately escape the process group/session may continue;
- removal of the active API-provider environment variable from direct command-child inheritance;
- best-effort exact-secret redaction from captured output, diagnostics, protocol output, and normal session writes;
- API-provider key values remain outside `config.toml`; Codex tokens use Lucy's private credential store and are not exposed as model-tool environment variables;
- private configuration, credential, and session files, with protected paths rejecting symlinks according to their respective storage implementations;
- append-only session records and bounded provider/SSE, command, and session fields where source constants impose limits;
- provider responses normalized into Lucy protocol events before public JSONL output;
- command-policy evaluation before process spawn when a policy is configured;
- background command output re-enters model context as a low-privilege observation/user input rather than system instructions.

A bound on captured output is not a bound on what a command can read or transmit. Redaction occurs after Lucy captures data and does not prevent a command from accessing or sending it elsewhere. Exact-secret matching does not reliably detect transformed, encoded, split, partial, or independently retrieved forms.

## Boundaries Lucy does not enforce

Lucy does not provide filesystem or network sandboxing, a container, a virtual machine, a separate OS user, repository-root confinement, semantic command analysis, guaranteed prompt-injection resistance, guaranteed protection of unrelated credentials, or guaranteed prevention of command-policy bypass. It does not coordinate or lock multiple processes writing the same persisted session.

Low-privilege observation framing and provider-role mapping reduce the authority Lucy assigns to command output. They do not guarantee that a model ignores malicious instructions embedded in data.

## Default-allow execution and deny policy

No configured policy means commands are allowed by default. A user-owned policy is evaluated before process spawn and can deny a command. Project instructions, skills, and model output cannot directly change Lucy's configured policy path. Because allowed commands run as the user, they may still modify an owner-writable policy file or its dependencies; protect policy files with external permissions or isolation when that matters.

A deny policy is a guardrail, not OS isolation. Equivalent effects may be reachable through another command, interpreter, script, alias, absolute path, shell feature, or indirect tool. Filtering command text cannot constrain the operating-system authority of an allowed process as strongly as a container, VM, sandbox, or dedicated user.

Policy configuration is described in [`README.md`](../README.md). `lucy doctor` reports command and provider runtime boundaries without modifying normal sessions; inspect the configured policy file separately.

## Credential model

### API-key providers

Lucy reads the key value from the environment-variable name stored in configuration. It does not write the value to `config.toml` or normal session records. The direct variable is removed before a model command is spawned. Output, diagnostics, sessions, and protocol records apply best-effort exact-secret redaction, with the limitations above.

### Codex subscription

OAuth access and refresh tokens are stored in a private Lucy credential file under the resolved XDG data/config boundary. Tokens are excluded from model tools, ordinary configuration, and persisted sessions. OAuth and refresh errors pass through credential-aware redaction. `lucy codex logout` removes the current credential file; it does not delete sessions or other local data.

### Unrelated credentials

SSH agents, Git credential helpers, cloud credentials, keychains, browser sessions, shell startup files, and other user authentication channels are outside Lucy's provider-secret boundary. A command running as the user may use them normally.

## Persistence and local data

Configuration is stored at `$XDG_CONFIG_HOME/lucy/config.toml`, falling back to `~/.config/lucy/config.toml`, with a private directory and `0600` file. Codex credentials use the private path reported by `lucy doctor`; the file contents must never be included in reports. Sessions are private append-only JSONL files under `~/.lucy/sessions/`.

A session may persist user and assistant messages, tool calls and bounded results, provider-setting audit records, the boot-context snapshot, discovered skill snapshots, provider reasoning metadata needed for replay, interruptions, and compaction summaries/boundaries. Session files are not encrypted. `lucy --list-sessions` lists resumable sessions. Users can inspect or delete their local JSONL files with ordinary filesystem tools.

Logout and provider-key rotation do not rewrite historical session data. Exact active-secret checks protect new reads and writes but do not erase data produced under an older credential. Resume uses the current provider settings with the historical boot snapshot. Concurrent writes to one session are unsupported.

## Recommended operating modes

1. **Trusted repository** — run Lucy directly as the normal user after reviewing its instructions and configuration.
2. **Partially trusted repository** — reduce unrelated credentials in the environment, enable a user-owned deny policy, inspect diffs, and avoid unattended destructive work.
3. **Untrusted repository or input** — run Lucy inside an external container, VM, sandbox, or dedicated OS account, and restrict network and credential access there.

External isolation constrains Lucy and every command it launches. That is stronger than command-text filtering and is the appropriate boundary for untrusted code or instructions.

## Maintenance

Review this contract whenever command execution, credentials, session persistence, JSONL protocol, setup/doctor behavior, or policy evaluation changes. The accepted architecture decisions in [`adr/records/harness/`](../adr/records/harness/) and their enforcement checks remain the durable source for those implementation boundaries.
