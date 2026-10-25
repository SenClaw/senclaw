#!/usr/bin/env bash
# Turn the failing tests of a `cargo test` log into workflow annotations. Reading
# a job's log on GitHub needs a sign-in; its annotations do not, so this is what
# says which test broke, and why, to anyone looking at the run.
#
# usage: annotate-test-failures.sh <log> [error|warning]
log=$1
level=${2:-error}

failed=$(grep -E '^test .+ \.\.\. FAILED' "$log" | sed -E 's/^test (.+) \.\.\. FAILED.*/\1/' | tr -d '\r' | paste -sd ' ' -)
[ -n "$failed" ] && echo "::$level title=Failing tests::$failed"

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
