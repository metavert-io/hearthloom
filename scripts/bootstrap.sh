#!/bin/bash
# Bootstrap a Hearthloom install: install + daemon (via install.sh where
# applicable), then create the ADMIN LOGIN — the moment the admin exists,
# every request to this daemon requires a session or an API key.
#
#   ./scripts/bootstrap.sh              local install (Mode A)
#   ./scripts/bootstrap.sh --remote     self-hosted always-on node (Mode B/D):
#                                       binds 0.0.0.0 — put TLS in front
#                                       (Fly edge / reverse proxy) before
#                                       exposing it beyond localhost.
#
# Credentials are read from stdin (never argv — argv leaks into `ps` and
# shell history). Runs once: an existing admin refuses a second bootstrap.
# Lost password recovery: set HEARTHLOOM_AUTH_OVERRIDE=open in
# ~/.hearthloom/.env and restart the daemon (shell access to that file is
# the same trust as the disk itself), sign in is bypassed while set.
set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
HL_HOME="${HEARTHLOOM_HOME:-$HOME/.hearthloom}"
PORT="${HEARTHLOOM_PORT:-43278}"
REMOTE=0
[ "${1:-}" = "--remote" ] && REMOTE=1

# ————— 1. install binary + daemon —————
if [ "$(uname)" = "Darwin" ]; then
  "$REPO_DIR/scripts/install.sh"
else
  # Linux (Mode D server): build, install, systemd user unit.
  echo "building release binary…"
  # Build for the NATIVE target explicitly — the default toolchain host
# can be x86_64 (Rosetta) on Apple Silicon, which runs 5-30x slower.
TARGET="$(rustc -vV | sed -n 's/^host: //p')"
if [ "$(uname)" = "Darwin" ] && [ "$(uname -m)" = "arm64" ]; then
  TARGET="aarch64-apple-darwin"
fi
cargo build --release --target "$TARGET" --manifest-path "$REPO_DIR/Cargo.toml" -q
  BIN_DIR="${HEARTHLOOM_BIN_DIR:-$HOME/.local/bin}"
  mkdir -p "$BIN_DIR"
  install -m 755 "$REPO_DIR/target/$TARGET/release/hearthloom" "$BIN_DIR/hearthloom"
  "$BIN_DIR/hearthloom" init
  mkdir -p "$HOME/.config/systemd/user"
  cat > "$HOME/.config/systemd/user/hearthloom.service" <<UNIT
[Unit]
Description=Hearthloom daemon (sole writer of the record)
[Service]
ExecStart=$BIN_DIR/hearthloom daemon run
Restart=always
[Install]
WantedBy=default.target
UNIT
  systemctl --user daemon-reload
  systemctl --user enable --now hearthloom
fi

# ————— 2. remote posture —————
if [ "$REMOTE" = 1 ]; then
  touch "$HL_HOME/.env" && chmod 600 "$HL_HOME/.env"
  if ! grep -q "^HEARTHLOOM_HOST=" "$HL_HOME/.env"; then
    echo "HEARTHLOOM_HOST=0.0.0.0" >> "$HL_HOME/.env"
  fi
  echo "remote posture: daemon will bind 0.0.0.0:$PORT after restart."
  echo "  ⚠ the daemon speaks plain HTTP — terminate TLS in front of it"
  echo "    (Fly edge does this automatically; elsewhere use caddy/nginx)."
  echo "  backups: consider litestream on $HL_HOME/hearthloom.db"
fi

# ————— 3. wait for the daemon —————
echo -n "waiting for the daemon"
for _ in $(seq 1 30); do
  if curl -s -o /dev/null "http://127.0.0.1:$PORT/health"; then break; fi
  echo -n "."; sleep 1
done
echo

# ————— 4. create the admin (stdin only) —————
STATUS=$(curl -s "http://127.0.0.1:$PORT/v1/auth/me" | grep -o '"auth":"[a-z]*"' || true)
if [ "$STATUS" = '"auth":"required"' ]; then
  echo "an admin already exists — bootstrap runs once. Done."
  exit 0
fi
read -r -p "admin email: " HL_USER
read -r -s -p "password (min 8 chars): " HL_PASS; echo
read -r -s -p "again: " HL_PASS2; echo
if [ "$HL_PASS" != "$HL_PASS2" ]; then echo "passwords differ — aborting."; exit 1; fi

COOKIES=$(mktemp)
trap 'rm -f "$COOKIES"' EXIT
RESP=$(printf '{"email":%s,"password":%s}' \
  "$(printf '%s' "$HL_USER" | python3 -c 'import json,sys;print(json.dumps(sys.stdin.read()))')" \
  "$(printf '%s' "$HL_PASS" | python3 -c 'import json,sys;print(json.dumps(sys.stdin.read()))')" \
  | curl -s -c "$COOKIES" -X POST "http://127.0.0.1:$PORT/v1/auth/bootstrap" \
      -H 'Content-Type: application/json' -d @-)
case "$RESP" in
  *bootstrapped*)
    echo "✓ admin $HL_USER created — signing in is now required." ;;
  *)
    echo "bootstrap failed: $RESP"; exit 1 ;;
esac

# The CLI talks the same locked HTTP API — mint its key while we still
# hold the bootstrap session, and stash it in .env (0600) where both the
# CLI and the daemon already read secrets from.
KEY=$(curl -s -b "$COOKIES" -X POST "http://127.0.0.1:$PORT/v1/keys" \
  -H 'Content-Type: application/json' \
  -d '{"actor":"human:bootstrap","name":"cli (minted at bootstrap)","scopes":["admin"]}' \
  | python3 -c 'import json,sys;print(json.load(sys.stdin).get("secret",""))')
if [ -n "$KEY" ]; then
  touch "$HL_HOME/.env" && chmod 600 "$HL_HOME/.env"
  echo "HEARTHLOOM_API_KEY=$KEY" >> "$HL_HOME/.env"
  echo "✓ CLI key minted and stored in $HL_HOME/.env"
else
  echo "⚠ couldn't mint the CLI key — mint one at ☰ → API Keys and add"
  echo "  HEARTHLOOM_API_KEY=<key> to $HL_HOME/.env or CLI calls will 401."
fi

if [ "$REMOTE" = 1 ]; then
  echo "restart the daemon to pick up the remote bind (launchctl kickstart / systemctl --user restart hearthloom)."
fi
echo "open http://127.0.0.1:$PORT/#/welcome — sign in and the onboarding wizard takes it from there."
