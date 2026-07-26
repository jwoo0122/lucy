#!/bin/sh

set -u

usage() {
    cat <<'EOF'
Usage: scripts/eval-capability-discovery.sh [--self-test] [--help]

Opt-in live-model evaluation of Lucy's project capability discovery.

  --self-test  Grade synthetic traces offline and exit. No provider is called,
               no credentials are read, and no Lucy process is started.
  --help       Show this message.

HIGH-RISK OPT-IN
  Without --self-test this evaluation is not a sandboxed test. Read this first:

  * It calls a REAL provider with the current user's Lucy configuration and
    REAL API credentials. Tokens are billed to that account.
  * It runs a real Lucy turn, and Lucy's `cmd` children INHERIT this process's
    environment (including secrets) and run with the invoking user's full
    access to $HOME and every other file that user can reach. That is the
    accepted trusted-shell boundary of Lucy, not a bug -- but it means the
    model's chosen commands execute with your privileges.
  * The prompt asks for read-only work inside a temporary project directory,
    but nothing constrains the model to it. Do not run this on a machine where
    an unexpected command would be costly.
  * A session transcript is written under ~/.lucy/sessions. On exit this
    harness attempts to remove the exact session file this run generated, and
    does not intentionally touch pre-existing sessions. Removal is best effort:
    it can be skipped if the session id cannot be read, if the run is killed
    outright, or if the file cannot be deleted.
  * That cleanup is only file removal after the turn. It does not sandbox the
    run: the model's commands keep the unrestricted access described above
    while the turn is running.

  Run it only when you intend all of the above.

  On failure the harness prints the evaluator's report plus two separately
  labelled bounded excerpts: Lucy's own stderr, and this harness's evaluator
  stderr. Lucy redacts its own diagnostics, but both excerpts are machine
  output that has not been reviewed here -- treat them accordingly.

Environment:
  LUCY_BIN  Lucy executable to evaluate (default: target/debug/lucy)

Exit status:
  0  evaluation passed
  1  evaluation failed
  2  the evaluation could not be set up
EOF
}

SCRIPT_DIR=$(CDPATH='' cd "$(dirname "$0")" && pwd)
EVALUATOR=$SCRIPT_DIR/eval-capability-discovery.py

if [ "$#" -gt 1 ]; then
    printf 'FAIL: this evaluation accepts at most one argument\n' >&2
    usage >&2
    exit 2
fi

SELF_TEST=0
case ${1-} in
    -h|--help)
        usage
        exit 0
        ;;
    --self-test)
        SELF_TEST=1
        ;;
    '')
        ;;
    *)
        printf 'FAIL: unknown argument: %s\n' "$1" >&2
        usage >&2
        exit 2
        ;;
esac

if ! command -v python3 >/dev/null 2>&1; then
    printf 'FAIL: python3 is required to parse the private JSONL capture\n' >&2
    exit 2
fi

if [ ! -f "$EVALUATOR" ]; then
    printf 'FAIL: evaluator not found: %s\n' "$EVALUATOR" >&2
    exit 2
fi

if [ "$SELF_TEST" -eq 1 ]; then
    exec python3 "$EVALUATOR" --self-test
fi

LUCY_BIN=${LUCY_BIN:-target/debug/lucy}
ORIGINAL_DIR=$(pwd -P)

# Resolve first: a bare name is only meaningful after a PATH lookup, and a
# relative path must be absolute before the run subshell changes directory.
case $LUCY_BIN in
    /*) ;;
    */*)
        lucy_dir=$(CDPATH='' cd "$(dirname "$LUCY_BIN")" 2>/dev/null && pwd) || lucy_dir=''
        if [ -z "$lucy_dir" ]; then
            printf 'FAIL: could not resolve Lucy executable: %s\n' "$LUCY_BIN" >&2
            exit 2
        fi
        LUCY_BIN=$lucy_dir/$(basename "$LUCY_BIN")
        ;;
    *)
        resolved_lucy=$(command -v "$LUCY_BIN" 2>/dev/null || :)
        if [ -z "$resolved_lucy" ]; then
            printf 'FAIL: could not resolve Lucy executable: %s\n' "$LUCY_BIN" >&2
            exit 2
        fi
        case $resolved_lucy in
            /*)
                LUCY_BIN=$resolved_lucy
                ;;
            */*)
                resolved_dir=$(CDPATH='' cd "$ORIGINAL_DIR/$(dirname "$resolved_lucy")" 2>/dev/null && pwd) || resolved_dir=''
                if [ -z "$resolved_dir" ]; then
                    printf 'FAIL: could not resolve Lucy executable: %s\n' "$LUCY_BIN" >&2
                    exit 2
                fi
                LUCY_BIN=$resolved_dir/$(basename "$resolved_lucy")
                ;;
            *)
                printf 'FAIL: Lucy PATH entry is not a file path: %s\n' "$resolved_lucy" >&2
                exit 2
                ;;
        esac
        ;;
esac

if [ ! -x "$LUCY_BIN" ]; then
    printf 'FAIL: Lucy executable is not executable: %s\n' "$LUCY_BIN" >&2
    printf 'Build it separately or set LUCY_BIN to an existing executable.\n' >&2
    exit 2
fi

if [ -z "${HOME-}" ]; then
    printf 'FAIL: HOME must be set so the generated Lucy session can be cleaned up\n' >&2
    exit 2
fi

umask 077
EVAL_TMP=$(mktemp -d "${TMPDIR:-/tmp}/lucy-capability-eval.XXXXXX") || {
    printf 'FAIL: could not create temporary evaluation directory\n' >&2
    exit 2
}
PROJECT_DIR=$EVAL_TMP/project
BIN_DIR=$EVAL_TMP/bin
JSONL_FILE=$EVAL_TMP/lucy.jsonl
STDERR_FILE=$EVAL_TMP/lucy.stderr
EVALUATOR_STDERR_FILE=$EVAL_TMP/evaluator.stderr
SESSION_ID_FILE=$EVAL_TMP/session-id
PREEXISTING_FILE=$EVAL_TMP/preexisting-session-ids
REPORT_FILE=$EVAL_TMP/report
SESSION_DIR=$HOME/.lucy/sessions

: >"$PREEXISTING_FILE"
if [ -d "$SESSION_DIR" ]; then
    for session_path in "$SESSION_DIR"/*.jsonl; do
        [ -f "$session_path" ] || continue
        session_name=${session_path##*/}
        printf '%s\n' "${session_name%.jsonl}" >>"$PREEXISTING_FILE"
    done
fi

extract_session_id() {
    [ -s "$JSONL_FILE" ] || return 0
    [ -s "$SESSION_ID_FILE" ] && return 0
    python3 "$EVALUATOR" session-id "$JSONL_FILE" "$SESSION_ID_FILE" >/dev/null 2>&1 || :
}

# Invoked indirectly by the EXIT trap.
# shellcheck disable=SC2329
cleanup() {
    extract_session_id
    if [ -s "$SESSION_ID_FILE" ]; then
        session_id=$(sed -n '1p' "$SESSION_ID_FILE")
        case $session_id in
            ''|*[!A-Za-z0-9_.-]*)
                ;;
            *)
                if ! grep -F -x "$session_id" "$PREEXISTING_FILE" >/dev/null 2>&1; then
                    generated_session=$SESSION_DIR/$session_id.jsonl
                    if [ -f "$generated_session" ]; then
                        rm -f "$generated_session"
                    fi
                fi
                ;;
        esac
    fi
    rm -rf "$EVAL_TMP"
}

trap cleanup 0
trap 'exit 129' 1
trap 'exit 130' 2
trap 'exit 131' 3
trap 'exit 143' 15

mkdir -p "$PROJECT_DIR" "$BIN_DIR"

printf '%s\n' \
    '{' \
    '  "name": "orb-capability-eval",' \
    '  "private": true,' \
    '  "scripts": {' \
    '    "inspect-orb": "qzx-luma-probe inspect sample.orb"' \
    '  }' \
    '}' >"$PROJECT_DIR/package.json"

printf 'ORB\000\002\377\020sample\000payload\376' >"$PROJECT_DIR/sample.orb"

cat >"$BIN_DIR/qzx-luma-probe" <<'EOF'
#!/bin/sh

case ${1-} in
    --help)
        printf '%s\n' \
            'Usage: qzx-luma-probe inspect FILE' \
            'Inspect an ORB binary and report its format metadata.'
        ;;
    inspect)
        if [ "$#" -ne 2 ] || [ ! -f "$2" ]; then
            printf 'qzx-luma-probe: inspect requires an existing file\n' >&2
            exit 2
        fi
        printf '%s\n' \
            'format: ORB/2' \
            'channels: 7' \
            'checksum: 7f3a91c2'
        ;;
    *)
        printf 'qzx-luma-probe: use --help for usage\n' >&2
        exit 2
        ;;
esac
EOF
chmod 700 "$BIN_DIR/qzx-luma-probe"

PROMPT='Read-only task: choose an available project-declared capability relevant to inspecting sample.orb. Inspect the project entry points, verify that the chosen capability is available, and inspect its current local help or usage before use. Only then, execute the underlying chosen executable directly to inspect sample.orb, as a single plain command on its own with no fallback, pipeline, redirect, or other command combined with it, then report the exact format, channel count, and checksum from its output. Do not modify any project file.'

lucy_status=0
printf '%s\n' "{\"type\":\"message\",\"text\":\"$PROMPT\"}" |
    (
        cd "$PROJECT_DIR" &&
            PATH=$BIN_DIR:$PATH "$LUCY_BIN" --jsonl >"$JSONL_FILE" 2>"$STDERR_FILE"
    ) || lucy_status=$?

extract_session_id

python3 "$EVALUATOR" report \
    "$JSONL_FILE" "$SESSION_ID_FILE" "$REPORT_FILE" "$lucy_status" \
    2>"$EVALUATOR_STDERR_FILE"
parse_status=$?

if [ -f "$REPORT_FILE" ]; then
    cat "$REPORT_FILE"
else
    printf 'FAIL: capability discovery result could not be parsed\n'
    parse_status=1
fi

# Diagnostic evidence is printed here, before the EXIT trap discards the
# temporary directory. The report carries only protocol messages plus call
# positions and status labels; model command text is omitted. The two stderr
# streams below have different provenance, so they are captured and labelled
# separately, and each is bounded by both lines and bytes.
STDERR_LINES=40
STDERR_BYTES=8192

# `wc -l` counts newlines, so a final unterminated line would be dropped from
# the count and silently omitted from the "further lines" notice.
count_lines() {
    awk 'END { print NR }' "$1"
}

print_stderr_excerpt() {
    excerpt_file=$1
    excerpt_label=$2
    printf '%s (up to %s lines/%s bytes):\n' \
        "$excerpt_label" "$STDERR_LINES" "$STDERR_BYTES"
    if [ -s "$excerpt_file" ]; then
        head -n "$STDERR_LINES" "$excerpt_file" | head -c "$STDERR_BYTES"
        excerpt_lines=$(count_lines "$excerpt_file")
        excerpt_bytes=$(wc -c <"$excerpt_file" | tr -d ' ')
        if [ "$excerpt_lines" -gt "$STDERR_LINES" ] || \
            [ "$excerpt_bytes" -gt "$STDERR_BYTES" ]; then
            printf '\n... (additional stderr omitted)\n'
        fi
    else
        printf '(no stderr was captured)\n'
    fi
}

if [ "$parse_status" -ne 0 ]; then
    print_stderr_excerpt "$STDERR_FILE" \
        'Lucy stderr (redacted by Lucy, not reviewed here)'
    print_stderr_excerpt "$EVALUATOR_STDERR_FILE" \
        'Evaluator stderr (this harness'\''s own parser output)'
fi

exit "$parse_status"
