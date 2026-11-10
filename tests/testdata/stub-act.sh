#!/bin/sh
# A stand-in for `nektos/act`, checked into testdata for the runner tests
# (mact-f1w.8). No docker, no network, no act: the tests assert the *runner's*
# guarantees — argv exactness, the log tee, redaction, the timeout/cancel kill —
# and this script is the fake act that lets them do it in milliseconds.
#
# Everything is driven by environment variables (set through
# `RunSpec::process_env`, which is the operator seam the tests reuse), because the
# runner's argv is exactly ADR-0003 §7.2's and must not be polluted with
# test-only flags.
#
#   STUB_ARGV_FILE        write "$@", one token per line, then continue
#   STUB_CWD_FILE         write $PWD (proves act's cwd is the per-run scratch dir)
#   STUB_SECRET_FILE_PROBE  path of a --secret-file to copy the *fact* of into the log
#   STUB_LINES_FILE       file whose contents are copied to stdout verbatim
#   STUB_STDERR_FILE      file whose contents are copied to stderr verbatim
#   STUB_ECHO_SECRET      name of an env var whose value is printed inside a JSON line
#                         (the redaction test: the runner must eat it before storage)
#   STUB_REPEAT N         print N padding-bearing JSON lines (log-cap test)
#   STUB_NO_NEWLINE       print bytes with no trailing newline (partial last line)
#   STUB_CHILD_SECS N     background a `sleep N` (proves the whole group is killed)
#   STUB_IGNORE_TERM      trap '' TERM, so SIGKILL escalation is exercised
#   STUB_KILL_SELF SIG    kill -SIG $$ (the act_crashed path)
#   STUB_SLEEP N          sleep N (timeout test)
#   STUB_EXIT_CODE N      exit status (default 0)

set -u

emit() {
  printf '%s\n' "$1"
}

if [ -n "${STUB_ARGV_FILE:-}" ]; then
  : > "$STUB_ARGV_FILE" || exit 70
  for arg in "$@"; do
    printf '%s\n' "$arg" >> "$STUB_ARGV_FILE" || exit 70
  done
fi

if [ -n "${STUB_CWD_FILE:-}" ]; then
  pwd > "$STUB_CWD_FILE" || exit 70
fi

# act-mcp must never hand us a secret in argv; prove the secret arrived by *file* by
# logging the file's key names only (never its values).
if [ -n "${STUB_SECRET_FILE_PROBE:-}" ]; then
  if [ -f "${STUB_SECRET_FILE_PROBE}" ]; then
    keys=$(cut -d= -f1 "${STUB_SECRET_FILE_PROBE}" | tr '\n' ',')
    emit "{\"level\":\"info\",\"msg\":\"secret file keys: ${keys}\"}"
  else
    emit "{\"level\":\"warning\",\"msg\":\"secret file missing\"}"
  fi
fi

if [ -n "${STUB_LINES_FILE:-}" ]; then
  cat "$STUB_LINES_FILE"
fi

if [ -n "${STUB_ECHO_SECRET:-}" ]; then
  value=$(printenv "$STUB_ECHO_SECRET" || true)
  emit "{\"level\":\"info\",\"msg\":\"token is ${value}\",\"time\":\"2026-11-10T00:42:03Z\"}"
fi

if [ -n "${STUB_REPEAT:-}" ]; then
  pad=$(printf 'x%.0s' $(seq 1 120))
  i=0
  while [ "$i" -lt "$STUB_REPEAT" ]; do
    i=$((i + 1))
    emit "{\"level\":\"info\",\"msg\":\"line ${i} ${pad}${pad}${pad}${pad}${pad}${pad}${pad}${pad}\"}"
  done
fi

if [ -n "${STUB_STDERR_FILE:-}" ]; then
  cat "$STUB_STDERR_FILE" >&2
fi

if [ -n "${STUB_NO_NEWLINE:-}" ]; then
  printf '%s' "$STUB_NO_NEWLINE"
fi

if [ -n "${STUB_CHILD_SECS:-}" ]; then
  # A grandchild in the same process group, like act's docker clients and containers.
  sleep "$STUB_CHILD_SECS" &
fi

if [ -n "${STUB_IGNORE_TERM:-}" ]; then
  trap '' TERM INT
fi

if [ -n "${STUB_KILL_SELF:-}" ]; then
  kill "-${STUB_KILL_SELF}" $$
fi

if [ -n "${STUB_SLEEP:-}" ]; then
  sleep "$STUB_SLEEP"
fi

exit "${STUB_EXIT_CODE:-0}"
