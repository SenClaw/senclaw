#!/bin/zsh
S=$(cd "$(dirname "$0")" && pwd)/lan
[ -f $S/daemon.pid ] && kill $(cat $S/daemon.pid) 2>/dev/null; sleep 2
[ -f $S/daemon.pid ] && kill -9 $(cat $S/daemon.pid) 2>/dev/null
lsof -nP -iTCP:48798 -iTCP:48799 -sTCP:LISTEN 2>/dev/null | tail -n +2
echo down
