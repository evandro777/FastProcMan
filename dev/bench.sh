#!/usr/bin/env bash
# Measures startup time, memory (PSS and RSS) and CPU of one GUI app, summed over its whole process tree.
# usage: dev/bench.sh <label> <command...>
# prints: <label> startup_ms=.. pss_mb=.. rss_mb=.. cpu=.. procs=..   (cpu = % of one core)
# Run it on the real X11 session with an otherwise idle desktop; repeat and take the median.
label=$1; shift
HZ=$(getconf CLK_TCK)
SETTLE=10; WINDOW=60

tree() { # all descendants of $1 plus itself
  local want=" $1 " out
  while :; do
    out=$(awk -v w="$want" '{ s=$0; sub(/^.*\) /,"",s); split(s,f," "); pid=$1; ppid=f[2];
      if (index(w," "ppid" ") && !index(w," "pid" ")) print pid }' /proc/[0-9]*/stat 2>/dev/null | tr '\n' ' ')
    [ -z "$out" ] && break
    want="$want$out "
  done
  echo $want
}
ticks() { for p in $1; do awk '{ s=$0; sub(/^.*\) /,"",s); split(s,f," "); print f[12]+f[13] }' /proc/$p/stat 2>/dev/null; done | paste -sd+ | bc; }
pss() { for p in $1; do awk '/^Pss:/{print $2}' /proc/$p/smaps_rollup 2>/dev/null; done | paste -sd+ | bc; }
rss() { for p in $1; do awk '/^VmRSS:/{print $2}' /proc/$p/status 2>/dev/null; done | paste -sd+ | bc; }

t0=$(date +%s%N)
"$@" >/dev/null 2>&1 &
pid=$!
# startup = until any process of the tree owns a visible window
for i in $(seq 1 200); do
  for p in $(tree $pid); do
    xdotool search --onlyvisible --pid $p >/dev/null 2>&1 && break 2
  done
  sleep 0.05
done
t1=$(date +%s%N)
startup=$(( (t1 - t0) / 1000000 ))
sleep $SETTLE
T=$(tree $pid)
a=$(ticks "$T"); sleep $WINDOW; T2=$(tree $pid); b=$(ticks "$T")
# multiply before dividing: bc truncates at `scale`, so dividing first would round CPU to whole percents
cpu=$(echo "scale=2; ($b - $a) * 100 / ($HZ * $WINDOW)" | bc)
echo "$label startup_ms=$startup pss_mb=$(echo "scale=1; $(pss "$T2")/1024" | bc) rss_mb=$(echo "scale=1; $(rss "$T2")/1024" | bc) cpu=$cpu procs=$(echo $T2 | wc -w)"
# close cleanly, then kill leftovers by PID (never pkill -f)
for p in $(tree $pid); do xdotool search --onlyvisible --pid $p 2>/dev/null | while read w; do wmctrl -ic $w; done; done
sleep 2
for p in $(tree $pid); do kill $p 2>/dev/null; done
sleep 1
