#!/usr/bin/env bash
# Turn the failing tests of a `cargo test` log into workflow annotations. Reading
# a job's log on GitHub needs a sign-in; its annotations do not, so this is what
# says which test broke, and why, to anyone looking at the run.
#
# usage: annotate-test-failures.sh <log> [error|warning]
level=${2:-error}
# Cargo colours its output on a runner; `^error` never matches `ESC[1m…error`.
log=$(mktemp)
trap 'rm -f "$log"' EXIT
sed -e 's/\x1b\[[0-9;]*m//g' "$1" > "$log"

esc() { sed -e 's/%/%25/g' -e 's/\r//g'; }

failed=$(grep -E '^test .+ \.\.\. FAILED' "$log" | sed -E 's/^test (.+) \.\.\. FAILED.*/\1/' | tr -d '\r' | paste -sd ' ' -)
[ -n "$failed" ] && echo "::$level title=Failing tests::$failed"

# Exit code 101 with no panic in the log: the tests did not compile, or the test
# binary died (a stack overflow aborts it without a word from the harness).
if ! grep -q 'panicked at' "$log"; then
  # The error line, then the ` --> file:line` under it.
  compile=$(grep -m 6 -A 1 -E '^error\[' "$log" | grep -E '^error\[|-->' | esc | paste -sd ' ' -)
  [ -n "$compile" ] && echo "::$level title=Did not compile::$compile"
  # One test thread: the last test that started and never reported is the one
  # that was running when the binary died.
  running=$(grep -E '^test .+ \.\.\. ' "$log" | tr -d '\r' | grep -vE ' \.\.\. (ok|FAILED|ignored)' | tail -1 | sed -E 's/^test (.+) \.\.\. .*/\1/')
  [ -n "$running" ] && echo "::$level title=Running when the test binary died::$running"
  tail_lines=$(grep -v '^[[:space:]]*$' "$log" | tail -4 | esc | paste -sd ' ' -)
  echo "::$level title=End of the log::$tail_lines"
fi

# A panic is `thread 'name' (id) panicked at file:line:col:` and its message on
# the next line. GitHub keeps ten annotations per step.
awk -v lvl="$level" '
  want {
    gsub(/\r/, ""); gsub(/%/, "%25")
    # A property value may not hold `:` or `,` as is, and a test path is full of `::`.
    title = name
    gsub(/%/, "%25", title); gsub(/:/, "%3A", title); gsub(/,/, "%2C", title)
    printf "::%s title=%s::%s (at %s)\n", lvl, title, $0, where
    want = 0
    next
  }
  /panicked at/ {
    name = "panic"
    if (match($0, /thread \047[^\047]*\047/)) name = substr($0, RSTART + 8, RLENGTH - 9)
    where = $0
    sub(/.*panicked at /, "", where)
    sub(/:\r?$/, "", where)
    want = 1
  }
' "$log" | head -9
