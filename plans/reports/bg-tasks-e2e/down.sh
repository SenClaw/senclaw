#!/bin/zsh
# usage: down.sh <label> [kill9] — stops ONLY the pids this harness recorded
SP=${BG_RUNS:?set BG_RUNS}
S=$SP/run-$1
SIG=TERM; [ "${2:-}" = kill9 ] && SIG=KILL
[ -f $S/daemon.pid ] && kill -$SIG $(cat $S/daemon.pid) 2>/dev/null
[ -f $S/mock.pid ] && kill $(cat $S/mock.pid) 2>/dev/null
sleep 2
for f in daemon mock; do
  [ -f $S/$f.pid ] && kill -0 $(cat $S/$f.pid) 2>/dev/null && kill -9 $(cat $S/$f.pid)
done
echo stopped
