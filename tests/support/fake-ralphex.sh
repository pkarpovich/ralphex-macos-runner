#!/bin/sh
set -u

ignore_term="${FAKE_RALPHEX_IGNORE_TERM:-}"
if [ -n "$ignore_term" ]; then
  trap '' TERM
fi

record="${FAKE_RALPHEX_RECORD:-}"

if [ -n "$record" ]; then
  {
    for arg in "$@"; do
      echo "argv: $arg"
    done
    echo "cwd: $(pwd -P)"
    echo "pid: $$"
    env | while IFS= read -r entry; do
      echo "env: $entry"
    done
  } >"$record"
fi

lines="${FAKE_RALPHEX_LINES:-0}"
index=1
while [ "$index" -le "$lines" ]; do
  echo "out $index"
  echo "err $index" >&2
  index=$((index + 1))
done

burst="${FAKE_RALPHEX_BURST:-0}"
if [ "$burst" -gt 0 ]; then
  awk -v n="$burst" 'BEGIN { for (i = 1; i <= n; i++) printf "burst %d\n", i }'
fi

long="${FAKE_RALPHEX_LONG_LINE:-0}"
if [ "$long" -gt 0 ]; then
  printf '%*s' "$long" '' | tr ' ' 'L'
  printf '\n'
fi

if [ -n "${FAKE_RALPHEX_HOLD:-}" ]; then
  sleep "$FAKE_RALPHEX_HOLD" &
fi

if [ -n "${FAKE_RALPHEX_LATE:-}" ]; then
  {
    sleep "$FAKE_RALPHEX_LATE"
    echo "late"
  } &
fi

if [ -n "${FAKE_RALPHEX_CHILD:-}" ]; then
  sleep "$FAKE_RALPHEX_CHILD" >/dev/null 2>&1 &
  if [ -n "$record" ]; then
    echo "child: $!" >>"$record"
  fi
fi

if [ -n "${FAKE_RALPHEX_STUBBORN_CHILD:-}" ]; then
  sh -c '
    trap "" TERM
    while :; do
      sleep 0.05
    done
  ' >/dev/null 2>&1 &
  if [ -n "$record" ]; then
    echo "child: $!" >>"$record"
  fi
fi

if [ -n "$ignore_term" ]; then
  while :; do
    sleep 0.05
  done
fi

sleep "${FAKE_RALPHEX_SLEEP:-0}"

exit "${FAKE_RALPHEX_EXIT:-0}"
