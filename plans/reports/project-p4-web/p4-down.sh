#!/bin/zsh
S=${P4_RUNS:-/tmp/senclaw-p4-runs}/run-p4web   # stops ONLY the pids p4-up.sh recorded
for f in daemon mock; do [ -f $S/$f.pid ] && kill $(cat $S/$f.pid) 2>/dev/null; done
sleep 3
for f in daemon mock; do [ -f $S/$f.pid ] && kill -0 $(cat $S/$f.pid) 2>/dev/null && kill -9 $(cat $S/$f.pid); done
echo stopped
