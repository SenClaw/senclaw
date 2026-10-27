#!/bin/zsh
HERE=$(cd "$(dirname "$0")" && pwd)
S=$HERE/run
for f in daemon mock; do
  [ -f $S/$f.pid ] && kill $(cat $S/$f.pid) 2>/dev/null
done
sleep 2
for f in daemon mock; do
  [ -f $S/$f.pid ] && kill -9 $(cat $S/$f.pid) 2>/dev/null
done
lsof -nP -iTCP:48798 -iTCP:48799 -iTCP:48941 -sTCP:LISTEN 2>/dev/null | tail -n +2
echo down
