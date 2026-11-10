#!/usr/bin/env bash
# scripts/macos-capture-doctor.sh — explain why agent sessions cannot take screenshots.
#
#   bash scripts/macos-capture-doctor.sh [path/to/Keitora.app]   (default /Applications/Keitora.app)
#
# Read-only: never modifies anything and never calls sudo.
#
# Background: macOS checks Screen Recording against the *responsible process* of the
# caller. For `screencapture` run by an agent, that is the Keitora instance that started
# the long-lived `herdr server` the panes live in. The grant is stored against that app's
# designated requirement, so it only sticks when Keitora is signed with a stable identity,
# and the herdr server must have been started by such a build.
set -uo pipefail

APP="${1:-/Applications/Keitora.app}"
BUNDLE_ID="com.keitora.dispatch"
fails=0

ok() { echo "OK    $*"; }
warn() { echo "WARN  $*"; }
fail() {
  echo "FAIL  $*"
  fails=$((fails + 1))
}
info() { echo "      $*"; }

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This script is for macOS only." >&2
  exit 1
fi

echo "Keitora screen-capture doctor — $APP"
echo ""

# ── 1. App signature ──────────────────────────────────────────────────────────
BIN="$APP/Contents/MacOS/keitora-app"
if [[ ! -x "$BIN" ]]; then
  fail "no Keitora app at $APP"
else
  req=$(codesign -dr - "$APP" 2>&1)
  if [[ "$req" == *"identifier \"$BUNDLE_ID\""* ]]; then
    ok "app has a stable designated requirement (identifier \"$BUNDLE_ID\")"
  elif [[ "$req" == *'cdhash H"'* ]]; then
    fail "app is ad-hoc signed: its designated requirement is a cdhash"
    info "Screen Recording grants are tied to this exact build and are lost on the next one."
    info "Rebuild with APPLE_SIGNING_IDENTITY set (see CONTRIBUTING.md, \"macOS permissions\")."
  else
    fail "could not read a usable designated requirement:"
    info "$req"
  fi
  if codesign --verify --deep --strict "$APP" >/dev/null 2>&1; then
    ok "codesign --verify --deep --strict passes"
  else
    warn "codesign --verify --deep --strict fails (bundle not fully sealed)"
  fi
fi

# ── 2. Signing identities ─────────────────────────────────────────────────────
names=$(security find-identity -v -p codesigning 2>/dev/null | grep -oE '"[^"]+"' || true)
if [[ -n "$names" ]]; then
  ok "code-signing identities available for APPLE_SIGNING_IDENTITY:"
  while IFS= read -r n; do info "$n"; done <<<"$names"
else
  warn "no code-signing identities in the keychain"
  info "Create one with scripts/macos-local-signing-cert.sh."
fi

# ── 3. herdr server ───────────────────────────────────────────────────────────
# Exact argv match: pane bridges and `herdr server stop` also run herdr.
server_pid=""
while read -r pid args; do
  if [[ "$args" =~ (^|/)herdr\ server$ ]]; then
    server_pid=$pid
    break
  fi
done < <(ps -axww -o pid=,args=)

if [[ -z "$server_pid" ]]; then
  ok "no herdr server is running; the next Keitora launch starts one"
else
  started=$(LC_ALL=C ps -o lstart= -p "$server_pid" | sed 's/^ *//; s/ *$//')
  parent=$(ps -o ppid= -p "$server_pid" | tr -d ' ')
  parent_cmd=$(ps -o comm= -p "$parent" 2>/dev/null || echo "?")
  ok "herdr server pid $server_pid, started $started, parent $parent ($parent_cmd)"

  # ── 4. Stale server ─────────────────────────────────────────────────────────
  # A server started before this binary was installed was spawned by an older build, and
  # agent sessions are still attributed to that build. ctime, not mtime: copying the app
  # out of the dmg keeps the build-time mtime but sets a fresh ctime.
  if [[ -x "$BIN" ]]; then
    started_epoch=$(LC_ALL=C date -j -f "%a %b %d %T %Y" "$started" +%s 2>/dev/null || echo "")
    bin_epoch=$(stat -f %c "$BIN")
    if [[ -z "$started_epoch" ]]; then
      warn "could not parse the server start time"
    elif ((started_epoch < bin_epoch)); then
      warn "the herdr server is older than the installed Keitora; an earlier build started it"
      info "Agent sessions inherit that build's identity. Restart the server: quit Keitora,"
      info "run 'herdr server stop' (this ends live panes; they resume later), relaunch Keitora."
    else
      ok "the herdr server was started after this Keitora was installed"
    fi
  fi
fi

# ── 5. Next steps ─────────────────────────────────────────────────────────────
echo ""
echo "One-time re-grant after switching to a signed build:"
echo "  1. Quit Keitora, then run: herdr server stop"
echo "  2. tccutil reset ScreenCapture $BUNDLE_ID"
echo "  3. Launch Keitora and allow it under System Settings → Privacy & Security →"
echo "     Screen & System Audio Recording (Keitora, not herdr or your terminal)."
echo "Optional check of who macOS charges (needs a password, run it in a terminal):"
echo "  sudo launchctl procinfo <pid of a shell in a Keitora pane> | grep -i responsible"

echo ""
if ((fails > 0)); then
  echo "$fails problem(s) found."
  exit 1
fi
echo "No problems found."
