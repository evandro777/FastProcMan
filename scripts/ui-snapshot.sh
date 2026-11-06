#!/usr/bin/env bash
# Renders pages of the app on a private Xvfb display with canned data (FASTPROCMAN_FIXTURES) and saves PNGs to
# target/ui-snapshots/<page>-<mode>.png. The real system is never scanned or changed.
# Usage: scripts/ui-snapshot.sh [page ...]   (names or numbers; default all)   MODES="empty single many" to narrow.
set -euo pipefail
cd "$(dirname "$0")/.."

# Add one entry per sidebar page, with the number the app's `page` property uses.
declare -A PAGES=([processes]=0 [graphs]=1 [graphs-tall]=graphs-tall [about]=about [confirm]=confirm [confirm-kill]=confirm-kill [properties]=properties [menu]=menu [settings]=settings [tree]=tree [services]=2 [confirm-service]=confirm-service)
MODES=${MODES:-"empty single many"}
OUT=target/ui-snapshots
DISPLAY_NUM=:99
mkdir -p "$OUT"
# The app reads (and a toggle saves) settings.toml: a scratch folder keeps the owner's saved choices (Tree, window
# size, language) out of the snapshots, and the snapshots out of the owner's settings.
DATA=$(mktemp -d)

cargo build --quiet

Xvfb "$DISPLAY_NUM" -screen 0 1280x1200x24 >/dev/null 2>&1 &
XVFB=$!
APP=
cleanup() { [ -n "$APP" ] && kill "$APP" 2>/dev/null || true; kill "$XVFB" 2>/dev/null || true; rm -rf "$DATA"; }
trap cleanup EXIT
sleep 1

[ $# -gt 0 ] && names=("$@") || names=("${!PAGES[@]}")

for name in "${names[@]}"; do
  page=${PAGES[$name]:-$name}
  for mode in $MODES; do
    XDG_DATA_HOME=$DATA DISPLAY=$DISPLAY_NUM FASTPROCMAN_FIXTURES=$mode FASTPROCMAN_PAGE=$page target/debug/fastprocman >/dev/null 2>&1 &
    APP=$!
    for _ in $(seq 50); do
      DISPLAY=$DISPLAY_NUM xdotool search --name FastProcMan >/dev/null 2>&1 && break
      sleep 0.2
    done
    sleep 1
    DISPLAY=$DISPLAY_NUM import -window root "$OUT/$name-$mode.png"
    kill "$APP" 2>/dev/null || true
    wait "$APP" 2>/dev/null || true
    APP=
    echo "$OUT/$name-$mode.png"
  done
done
