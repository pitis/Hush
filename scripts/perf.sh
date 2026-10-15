#!/usr/bin/env bash
# Measures the installed app against the M1 budget: size < 40 MB, cold launch < 1 s, idle RAM
# < 100 MB, idle CPU ~0 %. Needs a linked Semaphore (launch ends when the chat list has loaded).
# Usage: scripts/perf.sh [path/to/Semaphore.app]   (default /Applications/Semaphore.app)
set -euo pipefail
APP="${1:-/Applications/Semaphore.app}"
EXE="$APP/Contents/MacOS/Semaphore"
[ -x "$EXE" ] || { echo "✗ no app at $APP (make release)" >&2; exit 1; }
now() { python3 -c 'import time; print(time.time())'; }

echo "size: app $(du -sh "$APP" | cut -f1), executable $(du -h "$EXE" | cut -f1)   (budget: < 40 MB)"

pkill -x Semaphore && sleep 2 || true
LOG="$(mktemp)"
# the app logs "conversations: N" (counts only) once its core is open and the chat list is loaded
log stream --style compact --predicate 'subsystem == "dev.pitis.semaphore" AND eventMessage BEGINSWITH "conversations:"' >"$LOG" &
STREAM=$!
trap 'kill $STREAM 2>/dev/null || true; rm -f "$LOG"' EXIT
sleep 1
start=$(now)
open "$APP"
for _ in $(seq 1 300); do grep -q "conversations:" "$LOG" && break; sleep 0.02; done
end=$(now)
python3 -c "print(f'cold launch: {($end - $start):.2f} s   (budget: < 1 s; until the chat list is shown)')"

PID=$(pgrep -x Semaphore)
echo "idle: settling 30 s, then sampling for 60 s (leave the Mac alone)…"
sleep 30
rss=0; cpu=0; n=0
for _ in $(seq 1 12); do
  read -r r c < <(ps -o rss=,%cpu= -p "$PID")
  rss=$((rss + r)); cpu=$(python3 -c "print($cpu + $c)"); n=$((n + 1))
  sleep 5
done
python3 -c "print(f'idle memory: {$rss / $n / 1024:.0f} MB   (budget: < 100 MB)')"
python3 -c "print(f'idle CPU: {$cpu / $n:.1f} %   (budget: ~0 %)')"
