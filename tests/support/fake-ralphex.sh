#!/bin/sh
set -u

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

sleep "${FAKE_RALPHEX_SLEEP:-0}"

exit "${FAKE_RALPHEX_EXIT:-0}"
