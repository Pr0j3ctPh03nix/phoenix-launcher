#!/usr/bin/env bash
# Screenshot the frontend WITHOUT building the app.
#
#   bash dev/shoot.sh                       # every screen, at the configured window size
#   bash dev/shoot.sh confirm settings:files
#   SIZE=616,594 bash dev/shoot.sh main     # the configured minWidth/minHeight
#
# How: frontend/* is copied to dev/preview/.out/app, `preview/stub.js` (a canned
# window.__TAURI__) is injected before i18n.js and `preview/drive.js` (a location.hash director)
# after main.js, then headless Chrome renders each hash to a PNG in dev/preview/.out/.
# The real frontend is never touched — only the copy gets the two extra <script> tags.
#
# This is a LAYOUT check, not a behaviour one: the stub returns fixed data and nothing is wired
# to a real engine. To exercise the actual backend, use `bun run tauri dev`.
set -e
cd "$(dirname "$0")/.."

# Chrome is a native Windows binary: it needs a Windows-form path, and it cannot see /e/… .
# `pwd -W` is the MSYS/git-bash way to get one (E:/project-phoenix/…); forward slashes are fine.
ROOT="$(pwd -W 2>/dev/null || pwd)"
OUT="$ROOT/dev/preview/.out"
APP="$OUT/app"

CHROME=""
for c in "/c/Program Files/Google/Chrome/Application/chrome.exe" \
         "/c/Program Files (x86)/Microsoft/Edge/Application/msedge.exe" \
         "/c/Program Files/Microsoft/Edge/Application/msedge.exe"; do
  [ -x "$c" ] && CHROME="$c" && break
done
[ -n "$CHROME" ] || { echo "no Chrome or Edge found — install one or edit dev/shoot.sh"; exit 1; }

# Profiles are per-INVOCATION, not just per-screen. Reusing $OUT/profile-N across runs lets a
# previous run's cached state serve the page, and the symptom is a screenshot that silently
# reproduces the LAST run rather than this one — including a blank main from a run whose time
# budget was too short. Wiping them is cheap; debugging a stale PNG is not.
rm -rf "$OUT"/profile-*
mkdir -p "$APP"
cp frontend/index.html frontend/main.js frontend/i18n.js frontend/style.css "$APP/"
cp dev/preview/stub.js dev/preview/drive.js "$APP/"
[ -d "$APP/fonts" ] || cp -r frontend/fonts "$APP/fonts"   # ~1.4 MB, only copied once
sed -i 's|<script src="i18n.js"></script>|<script src="stub.js"></script>\n  <script src="i18n.js"></script>|' "$APP/index.html"
sed -i 's|<script src="main.js"></script>|<script src="main.js"></script>\n  <script src="drive.js"></script>|' "$APP/index.html"

SIZE="${SIZE:-825,740}"   # matches tauri.conf.json's window; SIZE=616,594 is its minimum
SCREENS="${*:-main main:open yours setup settings:general settings:launch settings:files options confirm confirm:keep confirm:restore confirm:restore2 gd verify manage update whatsnew whatsnew:launcher files files:kept files:doomed files:onlyextras}"
# UILANG=ru renders the Russian tables (the long labels). Not LANG — that is bash's own locale.
QUERY=""; SUFFIX=""
[ -n "$UILANG" ] && QUERY="?lang=$UILANG" && SUFFIX="-$UILANG"

i=0
for s in $SCREENS; do
  i=$((i + 1))
  name=$(echo "$s" | tr ':' '-')
  # --screenshot needs an ABSOLUTE path (a relative one silently writes nothing), and each run
  # needs its own --user-data-dir or back-to-back launches clobber each other and produce nothing.
  # The virtual-time budget has to comfortably outlast drive.js's 900 ms settle PLUS the .rise
  # reveal, or the capture lands mid-animation and main renders as an EMPTY COLUMN — everything on
  # it is a .rise element, so "blank page" is what a budget that is merely tight looks like. It
  # is virtual time, so a generous number costs nothing in wall clock.
  "$CHROME" --headless=new --disable-gpu --hide-scrollbars --force-device-scale-factor=1 \
    --user-data-dir="$OUT/profile-$i" --window-size="$SIZE" --virtual-time-budget=15000 \
    --screenshot="$OUT/$name$SUFFIX.png" "file:///$APP/index.html$QUERY#$s" 2>&1 | tail -1
done
