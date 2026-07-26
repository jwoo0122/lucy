#!/usr/bin/env python3
"""Evaluator for the opt-in Lucy capability-discovery evaluation.

The evaluation is graded from the private JSONL capture only. Every capability
category must be backed by a `cmd` tool_call whose *correlated* tool_result
(matched by id) actually succeeded. Assistant prose is never accepted as
evidence that a command ran.
"""

import json
import re
import sys
import tempfile
from pathlib import Path

USAGE = """\
Usage:
  eval-capability-discovery.py report <jsonl> <session-out> <report-out> <lucy-status>
  eval-capability-discovery.py session-id <jsonl> <session-out>
  eval-capability-discovery.py --self-test
  eval-capability-discovery.py --help

report      Grade a JSONL capture; writes the report file, exits 0 on PASS.
session-id  Extract the session id from a JSONL capture (used for cleanup).
--self-test Grade synthetic traces offline. No provider, no credentials.
"""

SESSION_ID_RE = re.compile(r"[A-Za-z0-9_.-]+")
EXPECTED_FORMAT = "ORB/2"
EXPECTED_CHECKSUM = "7f3a91c2"
CHANNELS_RE = re.compile(
    r"(?i)(?:\b(?:channels?|channel\s+count)\b[^0-9]{0,12}\b7\b|\b7\s+channels?\b)"
)

COMMAND_START = r"(?:^|[;\n&|()]\s*)"
INSPECTION_TOOL_RE = re.compile(
    COMMAND_START + r"(?:cat|sed|grep|rg|jq|head|tail|awk|less|more|node|python3?)\b"
)
AVAILABILITY_RES = (
    re.compile(COMMAND_START + r"command\s+-[vV]\s+qzx-luma-probe(?:[;&|()\s]|$)"),
    re.compile(COMMAND_START + r"which\s+qzx-luma-probe(?:[;&|()\s]|$)"),
    re.compile(COMMAND_START + r"type(?:\s+-[A-Za-z]+)?\s+qzx-luma-probe(?:[;&|()\s]|$)"),
)
HELP_RE = re.compile(
    COMMAND_START + r"qzx-luma-probe\s+--help(?:[;&|()\s]|$)"
)

# The inspect call is the one command whose output *is* the answer, so it is
# graded far more strictly than the discovery steps: the whole command must be
# exactly one bare inspect invocation.
# Any fallback (`||`), pipeline, redirect, substitution, or extra output
# command makes the observed stdout untrustworthy as proof of execution. This
# deliberately false-fails legitimate-but-elaborate wrappers; that is the
# accepted trade for never false-passing fabricated output.

# Every *attempt* to run the inspect capability is counted, whether or not it
# was bare and whether or not it succeeded. A run that probes the capability
# first and only discovers afterwards has already used it, so a later
# well-formed inspect cannot repair the ordering. PASS therefore requires
# exactly one attempt in the whole capture.
#
# Attempt detection is deliberately broad and position-independent: the
# executable named anywhere together with an inspect verb counts, so
# `sh -c "qzx-luma-probe inspect sample.orb"` and a quoted executable are
# attempts, as is running the package script that wraps the same call. Only
# `is_bare_inspect` stays exact -- broad detection here cannot weaken
# verification, it can only reveal an extra use of the capability.
INSPECT_VERB_RE = re.compile(r"(?i)\binspect\b")
SEGMENT_SPLIT_RE = re.compile(r"[;\n&|()]")
PACKAGE_RUNNER_RE = re.compile(
    r"(?i)^(?:(?:env(?:\s+(?:-[^\s]+|[A-Za-z_][A-Za-z0-9_]*=[^\s]+))*|"
    r"sh\s+-c)\s+['\"]?\s*)*(?:npm|pnpm|yarn|bun|npx)\b"
)
RUN_TARGET_RE = re.compile(r"(?i)\binspect-orb\b")


def runs_package_script(command):
    """True when a package runner in command position reaches `inspect-orb`.

    Option words sit between the runner and the script name (`npm --silent run
    inspect-orb`, `yarn --cwd . inspect-orb`), so the script name is accepted
    anywhere in the same shell command segment rather than at a fixed offset.
    The runner itself must still be in command position, optionally behind
    `env` or `sh -c`, which keeps `grep inspect-orb package.json` a discovery
    read and not an attempt.
    """
    for segment in SEGMENT_SPLIT_RE.split(command):
        segment = segment.strip()
        if PACKAGE_RUNNER_RE.match(segment) and RUN_TARGET_RE.search(segment):
            return True
    return False


def is_inspect_attempt(command):
    if "qzx-luma-probe" in command.lower() and INSPECT_VERB_RE.search(command):
        return True
    return runs_package_script(command)

# Discovery stdout must visibly show what the step claims to have discovered.
PROJECT_STDOUT_TOKENS = ("inspect-orb", "qzx-luma-probe")
AVAILABILITY_STDOUT_TOKENS = ("qzx-luma-probe",)
HELP_STDOUT_TOKENS = ("qzx-luma-probe", "Usage", "inspect")

# A command that already spells out an answer it is supposed to discover
# cannot be evidence of discovery. Command-position regexes above separately
# reject strings such as `echo qzx-luma-probe inspect sample.orb`.
FABRICATION_RES = (
    re.compile(re.escape(EXPECTED_FORMAT)),
    re.compile(re.escape(EXPECTED_CHECKSUM), re.IGNORECASE),
)


# Failure diagnostics are deliberately a narrow allow-list: protocol event
# types plus call positions and status labels. Free-form protocol text, model
# command text, tool stdout/stderr, and Lucy configuration are never included.
DIAGNOSTIC_EVENT_TYPES = ("error", "protocol_error", "turn_interrupted")
MAX_DIAGNOSTIC_ITEMS = 20


def collect_diagnostics(events, calls, attempts, attempts_ok):
    """Safe evidence: protocol errors plus command positions and statuses."""
    lines = []

    def add(line):
        if line not in lines and len(lines) < MAX_DIAGNOSTIC_ITEMS:
            lines.append(line)

    for event in events:
        event_type = event.get("type")
        if event_type not in DIAGNOSTIC_EVENT_TYPES:
            continue
        add(f"{event_type} event")

    if not attempts_ok:
        for position, _ in attempts:
            add(f"inspect attempt at call {position + 1}")

    for position, call in enumerate(calls):
        if call["result"] is None:
            label = "uncorrelated or unfinished command"
        elif not result_succeeded(call["result"]):
            label = "failed command"
        else:
            continue
        add(f"{label} at call {position + 1}")

    return lines


def is_fabricated(command):
    return any(pattern.search(command) for pattern in FABRICATION_RES)


def is_bare_inspect(command):
    """True only for the single exact inspect command requested by the fixture."""
    return command == "qzx-luma-probe inspect sample.orb"


def stdout_shows(stdout, tokens):
    return all(token in stdout for token in tokens)


def result_succeeded(result):
    """A cmd result counts only as a clean, completed, non-canceled success."""
    if not isinstance(result, dict):
        return False
    if result.get("exit_code") != 0:
        return False
    if result.get("timed_out") is True or result.get("canceled") is True:
        return False
    if result.get("error") is not None:
        return False
    return True


def result_stdout(result):
    stdout = result.get("stdout") if isinstance(result, dict) else None
    return stdout if isinstance(stdout, str) else ""


def load_events(path):
    """Return (events, parse_error). Any malformed line aborts the parse."""
    events = []
    try:
        with open(path, encoding="utf-8") as stream:
            for line_number, line in enumerate(stream, 1):
                if not line.strip():
                    continue
                try:
                    event = json.loads(line)
                except (TypeError, ValueError):
                    return events, f"invalid JSONL record at line {line_number}"
                if isinstance(event, dict):
                    events.append(event)
    except OSError:
        return events, "JSONL capture could not be read"
    return events, None


def find_session_id(events):
    for event in events:
        if event.get("type") != "session":
            continue
        candidate = event.get("session_id")
        if isinstance(candidate, str) and SESSION_ID_RE.fullmatch(candidate):
            return candidate
    return None


def collect_cmd_calls(events):
    """Correlate cmd calls/results and report protocol integrity.

    Duplicate call ids, unmatched/duplicate results, and unfinished calls make
    the trace invalid. Event indexes preserve completion and answer ordering.
    """
    calls = []
    by_id = {}
    integrity_ok = True
    for index, event in enumerate(events):
        event_type = event.get("type")
        if event_type == "tool_call" and event.get("name") == "cmd":
            call_id = event.get("id")
            arguments = event.get("arguments")
            if not isinstance(call_id, str) or not isinstance(arguments, str):
                integrity_ok = False
                continue
            try:
                decoded = json.loads(arguments)
            except (TypeError, ValueError):
                integrity_ok = False
                continue
            command = decoded.get("command") if isinstance(decoded, dict) else None
            if not isinstance(command, str) or call_id in by_id:
                integrity_ok = False
                continue
            call = {
                "id": call_id,
                "command": command,
                "result": None,
                "index": index,
                "result_index": None,
            }
            calls.append(call)
            by_id[call_id] = call
        elif event_type == "tool_result" and event.get("name") == "cmd":
            call_id = event.get("id")
            call = by_id.get(call_id) if isinstance(call_id, str) else None
            if call is None or call["result"] is not None:
                integrity_ok = False
                continue
            call["result"] = event.get("result")
            call["result_index"] = index

    if any(call["result"] is None for call in calls):
        integrity_ok = False
    return calls, integrity_ok


def evaluate(events, parse_error, lucy_status):
    session_id = find_session_id(events)
    turn_end_indexes = [
        index for index, event in enumerate(events) if event.get("type") == "turn_end"
    ]
    terminal_turn_end_index = (
        turn_end_indexes[0]
        if len(turn_end_indexes) == 1 and turn_end_indexes[0] == len(events) - 1
        else None
    )

    calls, cmd_protocol_integrity_ok = collect_cmd_calls(events)
    project_label = "project entry point inspected"
    availability_label = "qzx-luma-probe availability inspected"
    help_label = "qzx-luma-probe help inspected"
    inspect_label = "qzx-luma-probe inspect executed"
    verified = {
        project_label: [],
        availability_label: [],
        help_label: [],
        inspect_label: [],
    }
    # Successful discovery-like results are tracked separately from trusted
    # evidence. A command that embeds expected output is not evidence, but if
    # it finishes after inspect it still proves discovery was completed late.
    successful_discovery_result_indexes = []
    # Event index of the tool_result that verified the bare inspect. Only
    # assistant text emitted after it can count as reporting what was read.
    inspect_result_index = None
    # Call order is the order of the tool_call events, so the position of a
    # verified call is what proves discovery happened before execution.
    for position, call in enumerate(calls):
        command = call["command"]
        if not result_succeeded(call["result"]):
            continue
        stdout = result_stdout(call["result"])
        project_discovery = (
            "package.json" in command
            and INSPECTION_TOOL_RE.search(command)
            and stdout_shows(stdout, PROJECT_STDOUT_TOKENS)
        )
        availability_discovery = (
            "qzx-luma-probe" in command
            and any(pattern.search(command) for pattern in AVAILABILITY_RES)
            and stdout_shows(stdout, AVAILABILITY_STDOUT_TOKENS)
        )
        help_discovery = HELP_RE.search(command) and stdout_shows(
            stdout, HELP_STDOUT_TOKENS
        )
        if project_discovery or availability_discovery or help_discovery:
            successful_discovery_result_indexes.append(call["result_index"])
        if is_fabricated(command):
            continue
        if project_discovery:
            verified[project_label].append((position, command, call["result_index"]))
        if availability_discovery:
            verified[availability_label].append((position, command, call["result_index"]))
        if help_discovery:
            verified[help_label].append((position, command, call["result_index"]))
        if (
            is_bare_inspect(command)
            and EXPECTED_FORMAT in stdout
            and CHANNELS_RE.search(stdout)
            and EXPECTED_CHECKSUM in stdout.lower()
        ):
            verified[inspect_label].append((position, command, call["result_index"]))
            if inspect_result_index is None:
                inspect_result_index = call["result_index"]

    # Only the terminal response counts as the answer. It starts after the
    # final completed cmd result and ends immediately before the terminal
    # turn_end. Earlier post-inspect prose is intermediate reasoning/output,
    # and records after turn_end are malformed rather than part of the answer.
    cmd_result_indexes = [
        call["result_index"] for call in calls if call["result_index"] is not None
    ]
    last_cmd_result_index = max(cmd_result_indexes) if cmd_result_indexes else None
    final_answer_range_is_valid = (
        inspect_result_index is not None
        and last_cmd_result_index is not None
        and terminal_turn_end_index is not None
        and inspect_result_index <= last_cmd_result_index < terminal_turn_end_index
    )
    assistant_text = (
        ""
        if not final_answer_range_is_valid
        else "".join(
            event.get("text", "")
            for event in events[last_cmd_result_index + 1 : terminal_turn_end_index]
            if event.get("type") == "assistant_delta"
            and isinstance(event.get("text"), str)
        )
    )

    attempts = [
        (position, call["command"])
        for position, call in enumerate(calls)
        if is_inspect_attempt(call["command"])
    ]
    attempts_ok = len(attempts) == 1

    discovery_labels = (project_label, availability_label, help_label)
    inspect_call = next(
        (call for call in calls if is_bare_inspect(call["command"])), None
    )
    if (
        attempts_ok
        and cmd_protocol_integrity_ok
        and inspect_call is not None
        and all(verified[label] for label in discovery_labels)
    ):
        # Discovery is complete only when every observed successful discovery
        # result, including an untrusted/fabricated one, has arrived before the
        # one inspect call is issued.
        ordered = (
            all(
                result_index < inspect_call["index"]
                for result_index in successful_discovery_result_indexes
            )
            and any(
                position == attempts[0][0]
                for position, _, _ in verified[inspect_label]
            )
        )
    else:
        ordered = False

    checks = {
        "Lucy exited successfully": str(lucy_status) == "0",
        "JSONL parsed": parse_error is None,
        "session event exists": session_id is not None,
        "exactly one terminal turn_end exists": terminal_turn_end_index is not None,
        "cmd call/result protocol is complete and unique": cmd_protocol_integrity_ok,
        "assistant reports ORB/2": EXPECTED_FORMAT in assistant_text,
        "assistant reports channels 7": bool(CHANNELS_RE.search(assistant_text)),
        "assistant reports checksum": EXPECTED_CHECKSUM in assistant_text.lower(),
    }
    for label, entries in verified.items():
        checks[label] = bool(entries)
    checks["exactly one qzx-luma-probe inspect attempt"] = attempts_ok
    checks["discovery preceded qzx-luma-probe inspect"] = ordered

    evidence = []
    for label, entries in verified.items():
        for position, _, _ in entries:
            item = f"{label} at call {position + 1}"
            if item not in evidence:
                evidence.append(item)

    return {
        "session_id": session_id,
        "checks": checks,
        "passed": all(checks.values()),
        "parse_error": parse_error,
        "verified_evidence": evidence,
        "diagnostics": collect_diagnostics(events, calls, attempts, attempts_ok),
    }


def render_report(outcome):
    lines = []
    lines.append(
        "PASS: capability discovery live-model evaluation"
        if outcome["passed"]
        else "FAIL: capability discovery live-model evaluation"
    )
    if not outcome["passed"]:
        for label, ok in outcome["checks"].items():
            if not ok:
                lines.append(f"- missing: {label}")
        if outcome["parse_error"] is not None:
            lines.append(f"- parser: {outcome['parse_error']}")
        lines.append(
            "Diagnostics (protocol messages plus call positions/status only; "
            "command text, tool stdout/stderr, and Lucy configuration are omitted):"
        )
        if outcome["diagnostics"]:
            for line in outcome["diagnostics"]:
                lines.append(f"- {line}")
        else:
            lines.append("- none")
    lines.append("Verified cmd evidence:")
    if outcome["verified_evidence"]:
        for item in outcome["verified_evidence"]:
            lines.append(f"- {item}")
    else:
        lines.append("- none")
    return "\n".join(lines) + "\n"


def write_session_id(session_path, session_id):
    if session_id is None:
        return
    with open(session_path, "w", encoding="utf-8") as output:
        output.write(session_id)


def command_report(argv):
    if len(argv) != 4:
        sys.stderr.write(USAGE)
        return 2
    jsonl_path, session_path, report_path, lucy_status = argv
    events, parse_error = load_events(jsonl_path)
    outcome = evaluate(events, parse_error, lucy_status)
    write_session_id(session_path, outcome["session_id"])
    with open(report_path, "w", encoding="utf-8") as report:
        report.write(render_report(outcome))
    return 0 if outcome["passed"] else 1


def command_session_id(argv):
    if len(argv) != 2:
        sys.stderr.write(USAGE)
        return 2
    jsonl_path, session_path = argv
    events, _ = load_events(jsonl_path)
    write_session_id(session_path, find_session_id(events))
    return 0


# --- self-test -------------------------------------------------------------

GOOD_STDOUT = "format: ORB/2\nchannels: 7\nchecksum: 7f3a91c2\n"
FINAL_ANSWER = "format ORB/2, channels: 7, checksum 7f3a91c2"
PACKAGE_STDOUT = '{"scripts": {"inspect-orb": "qzx-luma-probe inspect sample.orb"}}\n'
WHICH_STDOUT = "/tmp/bin/qzx-luma-probe\n"
HELP_STDOUT = "Usage: qzx-luma-probe inspect FILE\n"


def ok_result(command, stdout=""):
    return {
        "command": command,
        "exit_code": 0,
        "timed_out": False,
        "stdout": stdout,
        "stderr": "",
        "stdout_truncated": False,
        "stderr_truncated": False,
    }


def trace(calls, *, final_answer=FINAL_ANSWER, session=True, turn_end=True):
    """Build a synthetic JSONL trace. Each call is (command, result-or-None)."""
    events = []
    if session:
        events.append({"type": "session", "session_id": "self-test", "resumed": False})
    for index, (command, result) in enumerate(calls, 1):
        call_id = f"call-{index}"
        events.append(
            {
                "type": "tool_call",
                "id": call_id,
                "name": "cmd",
                "arguments": json.dumps({"command": command}),
            }
        )
        if result is not None:
            events.append(
                {"type": "tool_result", "id": call_id, "name": "cmd", "result": result}
            )
    if final_answer:
        events.append({"type": "assistant_delta", "text": final_answer})
    if turn_end:
        events.append({"type": "turn_end"})
    return events


def discovery_calls(inspect_command, inspect_result):
    return [
        ("cat package.json", ok_result("cat package.json", PACKAGE_STDOUT)),
        (
            "command -V qzx-luma-probe",
            ok_result("command -V qzx-luma-probe", WHICH_STDOUT),
        ),
        (
            "qzx-luma-probe --help",
            ok_result("qzx-luma-probe --help", HELP_STDOUT),
        ),
        (inspect_command, inspect_result),
    ]


def self_test_cases():
    inspect = "qzx-luma-probe inspect sample.orb"
    valid = trace(discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT)))

    failed_result = {
        "command": inspect,
        "exit_code": 127,
        "timed_out": False,
        "stdout": "",
        "stderr": "qzx-luma-probe: not found\n",
        "stdout_truncated": False,
        "stderr_truncated": False,
        "error": "command failed",
    }
    deceptive = trace(discovery_calls(inspect, failed_result))

    echoed = "echo 'format: ORB/2\nchannels: 7\nchecksum: 7f3a91c2'"
    fallback = f"{inspect} || cat expected-orb-metadata.txt"
    piped = f"{inspect} | tee orb.log"
    trailing = f"{inspect}; cat expected-orb-metadata.txt"
    guarded = f"set -eu\n{inspect}"
    wrapped = f"sh -c '{inspect}'"
    quoted = '"qzx-luma-probe" inspect sample.orb'
    run_target = "npm run inspect-orb"
    optioned_run_target = "npm --silent run inspect-orb"
    env_run_target = "env npm run inspect-orb"
    shell_run_target = "sh -c 'npm run inspect-orb'"

    failed_run_target = dict(failed_result, command=optioned_run_target)

    # The assistant states the expected values before inspect ever runs, and
    # says nothing afterwards.
    pre_inspect_answer = trace(
        discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT)), final_answer=None
    )
    pre_inspect_answer.insert(1, {"type": "assistant_delta", "text": FINAL_ANSWER})

    # The correct values appear after inspect, but another cmd result follows
    # and there is no final answer after that last result.
    intermediate_answer = trace(
        discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT)), final_answer=None
    )
    intermediate_answer.pop()  # rebuild the terminal segment explicitly
    intermediate_answer.append({"type": "assistant_delta", "text": FINAL_ANSWER})
    intermediate_answer.extend(
        [
            {
                "type": "tool_call",
                "id": "call-after-inspect",
                "name": "cmd",
                "arguments": json.dumps({"command": "pwd"}),
            },
            {
                "type": "tool_result",
                "id": "call-after-inspect",
                "name": "cmd",
                "result": ok_result("pwd", "/tmp/project\n"),
            },
            {"type": "turn_end"},
        ]
    )

    # A delta appended after turn_end is not a final answer in that turn.
    answer_after_turn_end = trace(
        discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT)), final_answer=None
    )
    answer_after_turn_end.append({"type": "assistant_delta", "text": FINAL_ANSWER})

    mixed_case_early_attempt = "QZX-LUMA-PROBE inspect sample.orb"

    duplicate_turn_end = list(valid)
    duplicate_turn_end.insert(len(duplicate_turn_end) - 1, {"type": "turn_end"})

    # All discovery calls are declared first, but their successful results do
    # not arrive until after inspect has already been issued and completed.
    discovery_results_after_inspect = [
        {"type": "session", "session_id": "self-test", "resumed": False},
        {
            "type": "tool_call",
            "id": "late-project",
            "name": "cmd",
            "arguments": json.dumps({"command": "cat package.json"}),
        },
        {
            "type": "tool_call",
            "id": "late-availability",
            "name": "cmd",
            "arguments": json.dumps({"command": "command -v qzx-luma-probe"}),
        },
        {
            "type": "tool_call",
            "id": "late-help",
            "name": "cmd",
            "arguments": json.dumps({"command": "qzx-luma-probe --help"}),
        },
        {
            "type": "tool_call",
            "id": "early-inspect",
            "name": "cmd",
            "arguments": json.dumps({"command": inspect}),
        },
        {
            "type": "tool_result",
            "id": "early-inspect",
            "name": "cmd",
            "result": ok_result(inspect, GOOD_STDOUT),
        },
        {
            "type": "tool_result",
            "id": "late-project",
            "name": "cmd",
            "result": ok_result("cat package.json", PACKAGE_STDOUT),
        },
        {
            "type": "tool_result",
            "id": "late-availability",
            "name": "cmd",
            "result": ok_result("command -v qzx-luma-probe", WHICH_STDOUT),
        },
        {
            "type": "tool_result",
            "id": "late-help",
            "name": "cmd",
            "result": ok_result("qzx-luma-probe --help", HELP_STDOUT),
        },
        {"type": "assistant_delta", "text": FINAL_ANSWER},
        {"type": "turn_end"},
    ]

    unmatched_result_after_answer = list(valid)
    unmatched_result_after_answer.insert(
        len(unmatched_result_after_answer) - 1,
        {
            "type": "tool_result",
            "id": "never-called",
            "name": "cmd",
            "result": ok_result("pwd", "/tmp/project\n"),
        },
    )

    duplicate_result = list(valid)
    inspect_result_position = next(
        index
        for index, event in enumerate(duplicate_result)
        if event.get("type") == "tool_result" and event.get("id") == "call-4"
    )
    duplicate_result.insert(inspect_result_position + 1, dict(duplicate_result[inspect_result_position]))

    late_fabricated_help = trace(
        discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT)), final_answer=None
    )
    late_fabricated_help.pop()
    late_help_command = "qzx-luma-probe --help # ORB/2"
    late_fabricated_help.extend(
        [
            {
                "type": "tool_call",
                "id": "late-fabricated-help",
                "name": "cmd",
                "arguments": json.dumps({"command": late_help_command}),
            },
            {
                "type": "tool_result",
                "id": "late-fabricated-help",
                "name": "cmd",
                "result": ok_result(late_help_command, HELP_STDOUT),
            },
            {"type": "assistant_delta", "text": FINAL_ANSWER},
            {"type": "turn_end"},
        ]
    )

    return [
        ("valid trace passes", valid, "0", True),
        ("deceptive failed inspect result is rejected", deceptive, "0", False),
        (
            "missing correlated tool_result is rejected",
            trace(discovery_calls(inspect, None)),
            "0",
            False,
        ),
        (
            "uncorrelated result for another id is rejected",
            trace(discovery_calls(inspect, None))
            + [
                {
                    "type": "tool_result",
                    "id": "call-99",
                    "name": "cmd",
                    "result": ok_result(inspect, GOOD_STDOUT),
                }
            ],
            "0",
            False,
        ),
        (
            "echoed output is rejected",
            trace(discovery_calls(echoed, ok_result(echoed, GOOD_STDOUT))),
            "0",
            False,
        ),
        (
            "tool name printed instead of executed is rejected",
            trace(
                discovery_calls(
                    "echo qzx-luma-probe inspect sample.orb",
                    ok_result("echo qzx-luma-probe inspect sample.orb", GOOD_STDOUT),
                )
            ),
            "0",
            False,
        ),
        (
            "hallucinated final answer with no cmd calls is rejected",
            trace([]),
            "0",
            False,
        ),
        (
            "inspect stdout missing the checksum is rejected",
            trace(
                discovery_calls(inspect, ok_result(inspect, "format: ORB/2\nchannels: 7\n"))
            ),
            "0",
            False,
        ),
        (
            "timed out inspect result is rejected",
            trace(
                discovery_calls(
                    inspect,
                    dict(ok_result(inspect, GOOD_STDOUT), exit_code=None, timed_out=True),
                )
            ),
            "0",
            False,
        ),
        (
            "canceled inspect result is rejected",
            trace(
                discovery_calls(
                    inspect,
                    dict(ok_result(inspect, GOOD_STDOUT), canceled=True),
                )
            ),
            "0",
            False,
        ),
        (
            "backgrounded inspect call is rejected",
            trace(
                discovery_calls(
                    inspect, {"background_id": "background-1", "command": inspect}
                )
            ),
            "0",
            False,
        ),
        (
            "missing final assistant values is rejected",
            trace(
                discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT)),
                final_answer="I inspected the file.",
            ),
            "0",
            False,
        ),
        (
            "nonzero Lucy exit status is rejected",
            valid,
            "1",
            False,
        ),
        (
            "missing turn_end is rejected",
            trace(discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT)), turn_end=False),
            "0",
            False,
        ),
        (
            "POSIX command -v is accepted",
            trace(
                [
                    ("cat package.json", ok_result("cat package.json", PACKAGE_STDOUT)),
                    (
                        "command -v qzx-luma-probe",
                        ok_result("command -v qzx-luma-probe", WHICH_STDOUT),
                    ),
                    (
                        "qzx-luma-probe --help",
                        ok_result("qzx-luma-probe --help", HELP_STDOUT),
                    ),
                    (inspect, ok_result(inspect, GOOD_STDOUT)),
                ]
            ),
            "0",
            True,
        ),
        (
            "inspect with a fabricating fallback is rejected",
            trace(discovery_calls(fallback, ok_result(fallback, GOOD_STDOUT))),
            "0",
            False,
        ),
        (
            "inspect piped through another command is rejected",
            trace(discovery_calls(piped, ok_result(piped, GOOD_STDOUT))),
            "0",
            False,
        ),
        (
            "inspect followed by an extra output command is rejected",
            trace(discovery_calls(trailing, ok_result(trailing, GOOD_STDOUT))),
            "0",
            False,
        ),
        (
            "inspect before discovery is rejected",
            trace(
                [
                    (inspect, ok_result(inspect, GOOD_STDOUT)),
                    ("cat package.json", ok_result("cat package.json", PACKAGE_STDOUT)),
                    (
                        "command -v qzx-luma-probe",
                        ok_result("command -v qzx-luma-probe", WHICH_STDOUT),
                    ),
                    (
                        "qzx-luma-probe --help",
                        ok_result("qzx-luma-probe --help", HELP_STDOUT),
                    ),
                ]
            ),
            "0",
            False,
        ),
        (
            "successful early inspect followed by discovery and a second "
            "successful inspect is rejected",
            trace(
                [(inspect, ok_result(inspect, GOOD_STDOUT))]
                + discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT))
            ),
            "0",
            False,
        ),
        (
            "failed early inspect followed by discovery and a valid inspect "
            "is rejected",
            trace(
                [(inspect, failed_result)]
                + discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT))
            ),
            "0",
            False,
        ),
        (
            "non-bare early inspect followed by a valid inspect is rejected",
            trace(
                [(piped, ok_result(piped, GOOD_STDOUT))]
                + discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT))
            ),
            "0",
            False,
        ),
        (
            "package script run before discovery and a valid inspect is rejected",
            trace(
                [(run_target, ok_result(run_target, GOOD_STDOUT))]
                + discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT))
            ),
            "0",
            False,
        ),
        (
            "failed optioned package script run before discovery and a valid "
            "inspect is rejected",
            trace(
                [(optioned_run_target, failed_run_target)]
                + discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT))
            ),
            "0",
            False,
        ),
        (
            "env-wrapped package script before a valid inspect is rejected",
            trace(
                [(env_run_target, ok_result(env_run_target, GOOD_STDOUT))]
                + discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT))
            ),
            "0",
            False,
        ),
        (
            "sh-wrapped package script before a valid inspect is rejected",
            trace(
                [(shell_run_target, ok_result(shell_run_target, GOOD_STDOUT))]
                + discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT))
            ),
            "0",
            False,
        ),
        (
            "final answer stated before the inspect result is rejected",
            pre_inspect_answer,
            "0",
            False,
        ),
        (
            "intermediate answer before the last cmd result is rejected",
            intermediate_answer,
            "0",
            False,
        ),
        (
            "assistant text after terminal turn_end is rejected",
            answer_after_turn_end,
            "0",
            False,
        ),
        (
            "an earlier turn_end before the terminal turn_end is rejected",
            duplicate_turn_end,
            "0",
            False,
        ),
        (
            "discovery results arriving after inspect starts are rejected",
            discovery_results_after_inspect,
            "0",
            False,
        ),
        (
            "unmatched cmd result after the answer is rejected",
            unmatched_result_after_answer,
            "0",
            False,
        ),
        (
            "duplicate cmd result is rejected",
            duplicate_result,
            "0",
            False,
        ),
        (
            "fabricated discovery result arriving after inspect is rejected",
            late_fabricated_help,
            "0",
            False,
        ),
        (
            "mixed-case early inspect attempt before a valid inspect is rejected",
            trace(
                [(mixed_case_early_attempt, ok_result(mixed_case_early_attempt, GOOD_STDOUT))]
                + discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT))
            ),
            "0",
            False,
        ),
        (
            "sh -c wrapped inspect before a valid inspect is rejected",
            trace(
                [(wrapped, ok_result(wrapped, GOOD_STDOUT))]
                + discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT))
            ),
            "0",
            False,
        ),
        (
            "quoted executable inspect before a valid inspect is rejected",
            trace(
                [(quoted, ok_result(quoted, GOOD_STDOUT))]
                + discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT))
            ),
            "0",
            False,
        ),
        (
            "final answer phrased as '7 channels' is accepted",
            trace(
                discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT)),
                final_answer="format ORB/2, 7 channels, checksum 7f3a91c2",
            ),
            "0",
            True,
        ),
        (
            "redirected project entry output is rejected",
            trace(
                [
                    (
                        "cat package.json >/dev/null",
                        ok_result("cat package.json >/dev/null", ""),
                    ),
                    (
                        "command -v qzx-luma-probe",
                        ok_result("command -v qzx-luma-probe", WHICH_STDOUT),
                    ),
                    (
                        "qzx-luma-probe --help",
                        ok_result("qzx-luma-probe --help", HELP_STDOUT),
                    ),
                    (inspect, ok_result(inspect, GOOD_STDOUT)),
                ]
            ),
            "0",
            False,
        ),
        (
            "redirected help output is rejected",
            trace(
                [
                    ("cat package.json", ok_result("cat package.json", PACKAGE_STDOUT)),
                    (
                        "command -v qzx-luma-probe",
                        ok_result("command -v qzx-luma-probe", WHICH_STDOUT),
                    ),
                    (
                        "qzx-luma-probe --help >/dev/null",
                        ok_result("qzx-luma-probe --help >/dev/null", ""),
                    ),
                    (inspect, ok_result(inspect, GOOD_STDOUT)),
                ]
            ),
            "0",
            False,
        ),
        (
            "semicolon set before a bare inspect is rejected",
            trace(
                discovery_calls(
                    f"set -eu; {inspect}",
                    ok_result(f"set -eu; {inspect}", GOOD_STDOUT),
                )
            ),
            "0",
            False,
        ),
        (
            "set -eu before a bare inspect is rejected",
            trace(discovery_calls(guarded, ok_result(guarded, GOOD_STDOUT))),
            "0",
            False,
        ),
    ]


def grade_to_report(root, name, events, lucy_status="0"):
    jsonl_path = root / f"{name}.jsonl"
    with open(jsonl_path, "w", encoding="utf-8") as stream:
        for event in events:
            stream.write(json.dumps(event) + "\n")
    report_path = root / f"{name}.report"
    command_report(
        [str(jsonl_path), str(root / f"{name}.session"), str(report_path), lucy_status]
    )
    return report_path.read_text(encoding="utf-8")


def diagnostic_self_test_cases(root):
    """Return (name, ok) pairs asserting what a failure report may contain."""
    inspect = "qzx-luma-probe inspect sample.orb"
    secret = "SUPERSECRET-TOOL-OUTPUT"
    failing_inspect = {
        "command": inspect,
        "exit_code": 3,
        "timed_out": False,
        "stdout": secret,
        "stderr": secret,
        "stdout_truncated": False,
        "stderr_truncated": False,
    }
    events = trace(discovery_calls(inspect, failing_inspect))
    events.insert(
        1,
        {"type": "error", "message": f"provider stream closed {secret}"},
    )
    events.append({"type": "turn_interrupted", "reason": f"user interrupt {secret}"})
    report = grade_to_report(root, "diagnostics", events)

    passing = grade_to_report(
        root, "diagnostics-pass", trace(discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT)))
    )

    repeated = grade_to_report(
        root,
        "diagnostics-repeat",
        trace(
            [(inspect, ok_result(inspect, GOOD_STDOUT))]
            + discovery_calls(inspect, ok_result(inspect, GOOD_STDOUT))
        ),
    )

    return [
        (
            "failure report lists every repeated inspect attempt separately",
            "inspect attempt at call 1" in repeated
            and "inspect attempt at call 5" in repeated,
        ),
        (
            "failure report includes protocol error type without free text",
            "error event" in report and "provider stream closed unexpectedly" not in report,
        ),
        (
            "failure report includes interruption type without free text",
            "turn_interrupted event" in report and "user interrupt" not in report,
        ),
        (
            "failure report includes failed command position without command text",
            "failed command at call" in report and inspect not in report,
        ),
        (
            "failure report never includes tool stdout or stderr",
            secret not in report,
        ),
        ("passing report emits no diagnostics section", "Diagnostics" not in passing),
    ]


def command_self_test():
    failures = 0
    with tempfile.TemporaryDirectory(prefix="lucy-capability-selftest.") as workdir:
        root = Path(workdir)
        for index, (name, events, lucy_status, expected_pass) in enumerate(self_test_cases()):
            jsonl_path = root / f"case-{index}.jsonl"
            with open(jsonl_path, "w", encoding="utf-8") as stream:
                for event in events:
                    stream.write(json.dumps(event) + "\n")
            status = command_report(
                [
                    str(jsonl_path),
                    str(root / f"case-{index}.session"),
                    str(root / f"case-{index}.report"),
                    lucy_status,
                ]
            )
            actual_pass = status == 0
            if actual_pass == expected_pass:
                print(f"ok   {name}")
            else:
                failures += 1
                expectation = "PASS" if expected_pass else "FAIL"
                print(f"FAIL {name} (expected {expectation})")
                print((root / f"case-{index}.report").read_text(encoding="utf-8").rstrip())

        # A malformed capture must not be silently graded as a pass.
        broken = root / "broken.jsonl"
        broken.write_text("not json\n", encoding="utf-8")
        if command_report(
            [str(broken), str(root / "broken.session"), str(root / "broken.report"), "0"]
        ) == 0:
            failures += 1
            print("FAIL malformed JSONL capture is rejected")
        else:
            print("ok   malformed JSONL capture is rejected")

        for name, ok in diagnostic_self_test_cases(root):
            if ok:
                print(f"ok   {name}")
            else:
                failures += 1
                print(f"FAIL {name}")

    if failures:
        print(f"FAIL: {failures} self-test case(s) failed")
        return 1
    print("PASS: capability discovery evaluator self-test")
    return 0


def main(argv):
    if not argv or argv[0] in ("-h", "--help"):
        sys.stdout.write(USAGE)
        return 0
    if argv[0] == "--self-test":
        if len(argv) != 1:
            sys.stderr.write(USAGE)
            return 2
        return command_self_test()
    if argv[0] == "report":
        return command_report(argv[1:])
    if argv[0] == "session-id":
        return command_session_id(argv[1:])
    sys.stderr.write(f"unknown argument: {argv[0]}\n")
    sys.stderr.write(USAGE)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
