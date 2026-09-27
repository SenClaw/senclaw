#!/usr/bin/env bash
# SenClaw one-line installer for macOS and Linux.
#
#   curl -fsSL https://raw.githubusercontent.com/SenClaw/senclaw/main/scripts/install.sh | bash
#
# Options (environment variables):
#   SENCLAW_VERSION=v0.3.0    install a specific release tag (default: latest)
#   SENCLAW_INSTALL_DIR=...   binary directory (default: ~/.senclaw/bin)
#
# Windows: use scripts/install.ps1 instead.
set -euo pipefail

REPO="SenClaw/senclaw"
VERSION="${SENCLAW_VERSION:-latest}"
INSTALL_DIR="${SENCLAW_INSTALL_DIR:-$HOME/.senclaw/bin}"

err() { echo "error: $*" >&2; exit 1; }

# ── Detect platform ──────────────────────────────────────────────────────────
case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux)  os="unknown-linux-gnu" ;;
  MINGW*|MSYS*|CYGWIN*)
    err "use the PowerShell installer on Windows:
  powershell -ExecutionPolicy Bypass -c \"irm https://raw.githubusercontent.com/$REPO/main/scripts/install.ps1 | iex\"" ;;
  *) err "unsupported OS: $(uname -s)" ;;
esac

case "$(uname -m)" in
  arm64|aarch64) arch="aarch64" ;;
  x86_64|amd64)  arch="x86_64" ;;
  *) err "unsupported architecture: $(uname -m)" ;;
esac

TARGET="${arch}-${os}"
if [ "$TARGET" = "aarch64-unknown-linux-gnu" ]; then
  err "no prebuilt binary for Linux arm64 yet — build from source:
  git clone https://github.com/$REPO.git && cd senclaw && cargo build --release"
fi

# ── Download ─────────────────────────────────────────────────────────────────
ASSET="senclaw-${TARGET}"
if [ "$VERSION" = "latest" ]; then
  URL="https://github.com/$REPO/releases/latest/download/$ASSET"
else
  URL="https://github.com/$REPO/releases/download/$VERSION/$ASSET"
fi

mkdir -p "$INSTALL_DIR"
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

echo "Downloading $URL"
curl -fSL --progress-bar "$URL" -o "$tmp" \
  || err "download failed — check https://github.com/$REPO/releases"

chmod +x "$tmp"
mv "$tmp" "$INSTALL_DIR/senclaw"
trap - EXIT
echo "Installed senclaw -> $INSTALL_DIR/senclaw"
"$INSTALL_DIR/senclaw" --version || true

# ── PATH setup ───────────────────────────────────────────────────────────────
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    line="export PATH=\"$INSTALL_DIR:\$PATH\""
    shell_name="$(basename "${SHELL:-sh}")"
    case "$shell_name" in
      zsh)  profile="$HOME/.zshrc" ;;
      bash) profile="$HOME/.bashrc" ;;
      fish) profile="" ;;
      *)    profile="$HOME/.profile" ;;
    esac
    if [ -n "$profile" ] && ! grep -qs "$INSTALL_DIR" "$profile"; then
      printf '\n# SenClaw\n%s\n' "$line" >> "$profile"
      echo "Added $INSTALL_DIR to PATH in $profile — restart your shell or run:"
      echo "  $line"
    else
      echo "Add senclaw to your PATH:"
      if [ "$shell_name" = "fish" ]; then
        echo "  fish_add_path $INSTALL_DIR"
      else
        echo "  $line"
      fi
    fi
    ;;
esac

cat <<'EOF'

Next steps:
  senclaw web              # download the Web UI + speech sidecar (first run) and start the daemon
  senclaw install desktop  # install the native desktop app
  senclaw --help           # all commands
EOF
