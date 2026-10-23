#!/bin/zsh
S=$(cd "$(dirname "$0")" && pwd)/real
for f in daemon mlx; do [ -f $S/$f.pid ] && kill $(cat $S/$f.pid) 2>/dev/null; done
sleep 3
for f in daemon mlx; do [ -f $S/$f.pid ] && kill -9 $(cat $S/$f.pid) 2>/dev/null; done
lsof -nP -iTCP:48788 -iTCP:48789 -iTCP:38931 -sTCP:LISTEN 2>/dev/null | tail -n +2
echo down
