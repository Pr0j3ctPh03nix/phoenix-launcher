#!/usr/bin/env bash
# Create a decoy game folder for testing the updater against, so a real Dota install is never touched.
#
# Layout produced (root = arg 1, default dev/decoy_game):
#   <root>/game/dota/steam.inf     the install-identity gate marker (ClientVersion)
#   <root>/game/dota/scripts/      regions.txt + matchgroups.txt land here
#   <root>/game/dota/cfg/          gc_client.cfg lands here
#   <root>/game/bin/win64/         winmm.dll lands here
#
# With the dirs empty, `check` reports every managed file as [install] and the gate as OK. Point the
# updater at it with:  cargo run -- check --game <root> --repo <owner/name>
#
# Pass a second arg to override the baked ClientVersion (e.g. to test a gate MISMATCH refusal).
set -e
ROOT="${1:-$(dirname "$0")/decoy_game}"
CLIENT_VERSION="${2:-1805}"

mkdir -p "$ROOT/game/dota/scripts" "$ROOT/game/dota/cfg" "$ROOT/game/bin/win64"
cat > "$ROOT/game/dota/steam.inf" <<EOF
ClientVersion=$CLIENT_VERSION
ServerVersion=$CLIENT_VERSION
appID=570
EOF

echo "decoy game folder ready: $ROOT  (ClientVersion=$CLIENT_VERSION)"
