#!/bin/zsh
# usage: env-down.sh <label> — stops ONLY the pids this harness recorded
SP=${P0_RUNS:-/tmp/senclaw-p0-runs}
S=$SP/run-$1
for f in daemon proxy; do
  [ -f $S/$f.pid ] && kill $(cat $S/$f.pid) 2>/dev/null
done
sleep 3
for f in daemon proxy; do
  [ -f $S/$f.pid ] && kill -0 $(cat $S/$f.pid) 2>/dev/null && kill -9 $(cat $S/$f.pid)
done
echo stopped
