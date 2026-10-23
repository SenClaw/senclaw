#!/bin/zsh
S=$(cd "$(dirname "$0")" && pwd)/run
for f in daemon mock; do
  [ -f $S/$f.pid ] && kill $(cat $S/$f.pid) 2>/dev/null
done
sleep 2
for f in daemon mock; do
  [ -f $S/$f.pid ] && kill -9 $(cat $S/$f.pid) 2>/dev/null
done
lsof -nP -iTCP:48788 -iTCP:48789 -iTCP:48931 -sTCP:LISTEN 2>/dev/null | tail -n +2
echo down
