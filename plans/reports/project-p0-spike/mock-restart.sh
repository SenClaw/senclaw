#!/bin/zsh
# usage: mock-restart.sh <label> — restart only the mock LLM of a run
HERE=$(cd "$(dirname "$0")" && pwd)
SP=${P0_RUNS:-/tmp/senclaw-p0-runs}
S=$SP/run-$1
kill $(cat $S/mock.pid) 2>/dev/null; sleep 1
cd $S/mock && nohup node $SP/mock-llm.mjs --port ${P0_MOCK:-38931} --out $S/mock > $S/log/mock.log 2>&1 &
echo $! > $S/mock.pid
sleep 1
