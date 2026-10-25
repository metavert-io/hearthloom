#!/bin/bash
# Install hearthloom: release build → ~/.local/bin, plus a launchd agent
# so the daemon (the substrate's sole writer) survives reboots.
set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BIN_DIR="${HEARTHLOOM_BIN_DIR:-$HOME/.local/bin}"
PLIST="$HOME/Library/LaunchAgents/com.hearthloom.daemon.plist"

echo "building release binary…"
# Build for the NATIVE target explicitly — the default toolchain host
# can be x86_64 (Rosetta) on Apple Silicon, which runs 5-30x slower.
TARGET="$(rustc -vV | sed -n 's/^host: //p')"
if [ "$(uname)" = "Darwin" ] && [ "$(uname -m)" = "arm64" ]; then
  TARGET="aarch64-apple-darwin"
fi
cargo build --release --target "$TARGET" --manifest-path "$REPO_DIR/Cargo.toml" -q

mkdir -p "$BIN_DIR"
install -m 755 "$REPO_DIR/target/$TARGET/release/hearthloom" "$BIN_DIR/hearthloom"
echo "installed $BIN_DIR/hearthloom"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "note: add $BIN_DIR to your PATH" ;;
esac

"$BIN_DIR/hearthloom" init

# Where the dashboard lives (HEARTHLOOM_DASHBOARD_URL; README, Configuration).
# Asked once, on a fresh install. An existing install keeps its settings
# (sealed in ~/.hearthloom/.env.sealed; change them through ~/.hearthloom/.env).
if [ ! -f "$HOME/.hearthloom/.env.sealed" ]; then
  DASHBOARD_URL="http://127.0.0.1:43278"
  if [ -t 0 ]; then
    printf 'Dashboard address [%s]: ' "$DASHBOARD_URL"
    read -r answer || answer=""
    [ -n "$answer" ] && DASHBOARD_URL="$answer"
  fi
  case "$DASHBOARD_URL" in *://*) ;; *) DASHBOARD_URL="http://$DASHBOARD_URL" ;; esac
  authority="${DASHBOARD_URL#*://}"; authority="${authority%%/*}"
  case "$authority" in
    \[*\]:*) host="${authority%%]*}]"; port="${authority##*]:}" ;;
    \[*\]) host="$authority"; port="" ;;
    *:*) host="${authority%:*}"; port="${authority##*:}" ;;
    *) host="$authority"; port="" ;;
  esac
  ENV_FILE="$HOME/.hearthloom/.env"
  touch "$ENV_FILE" && chmod 600 "$ENV_FILE"
  {
    echo "HEARTHLOOM_DASHBOARD_URL=$DASHBOARD_URL"
    if [ -n "$port" ] && [ "$port" != "43278" ]; then echo "HEARTHLOOM_PORT=$port"; fi
    case "$host" in 127.0.0.1|localhost|"[::1]") ;; *) echo "HEARTHLOOM_HOST=0.0.0.0" ;; esac
  } >> "$ENV_FILE"
  echo "dashboard: $DASHBOARD_URL"
  case "$host" in
    127.0.0.1|localhost|"[::1]") ;;
    *)
      echo "note: other machines on your network can reach $DASHBOARD_URL."
      echo "      Open it and set a password (the welcome screen offers one) before you add anything." ;;
  esac
fi

mkdir -p "$(dirname "$PLIST")"
cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.hearthloom.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>$BIN_DIR/hearthloom</string>
        <string>daemon</string>
        <string>run</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>StandardOutPath</key><string>$HOME/.hearthloom/daemon.log</string>
    <key>StandardErrorPath</key><string>$HOME/.hearthloom/daemon.log</string>
</dict>
</plist>
EOF

launchctl unload "$PLIST" 2>/dev/null || true
launchctl load "$PLIST"
echo "launchd agent loaded: com.hearthloom.daemon"
"$BIN_DIR/hearthloom" daemon status

# Make sure `hearthloom` is findable from the shell: add ~/.local/bin
# to PATH in the user's profile (idempotent — skipped when already set).
PROFILE="$HOME/.zshrc"
case "${SHELL:-}" in
  */bash) PROFILE="$HOME/.bashrc" ;;
esac
if ! grep -qs '\.local/bin' "$PROFILE" 2>/dev/null; then
  # never edit a shell profile without a yes (no terminal = no edit)
  answer=n
  if [ -t 0 ]; then
    printf 'Add ~/.local/bin to PATH in %s? [y/N] ' "$PROFILE"
    read -r answer || answer=n
  fi
  case "$answer" in
    y|Y|yes|YES)
      {
        echo ""
        echo "# hearthloom CLI"
        echo 'export PATH="$HOME/.local/bin:$PATH"'
      } >> "$PROFILE"
      echo "added ~/.local/bin to PATH in $PROFILE (open a new shell to pick it up)" ;;
    *)
      echo "to run hearthloom by name, add this line to $PROFILE:"
      echo '  export PATH="$HOME/.local/bin:$PATH"' ;;
  esac
else
  echo "PATH already carries ~/.local/bin ($PROFILE)"
fi
echo
echo "Optional settings (an ember registry, a hosting provisioner, HearthFS)"
echo "go in ~/.hearthloom/.env; see README, Configuration."
