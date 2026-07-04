#!/usr/bin/env sh
# shellcheck shell=dash
#
# SPDX-License-Identifier: AGPL-3.0-only
#
# Installs the Stalwart *free edition* (all formerly-enterprise features, built
# WITHOUT the `enterprise` feature) from a pre-compiled CI/CD artifact.
#
# Usage (as root):
#   curl -fsSL https://raw.githubusercontent.com/darwinva97/stalwart/free-edition/deploy/install-free-edition.sh | sh
#
# Env overrides:
#   REPO=darwinva97/stalwart      GitHub repo to fetch the release from
#   RELEASE_TAG=free-latest       Release tag to download (or a v* tag)
#
set -eu

REPO="${REPO:-darwinva97/stalwart}"
RELEASE_TAG="${RELEASE_TAG:-free-latest}"

BIN_DIR="/usr/local/bin"
CONF_DIR="/etc/stalwart"
DATA_DIR="/var/lib/stalwart"
LOG_DIR="/var/log/stalwart"
ACCOUNT="stalwart"
CONFIG_FILE="${CONF_DIR}/config.json"

say()  { printf '%s\n' "$*"; }
err()  { printf '❌ %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

[ "$(id -u)" -eq 0 ] || err "This installer must run as root."
have tar || err "'tar' is required."
have uname || err "'uname' is required."
if have curl; then DL="curl -fsSL -o"; elif have wget; then DL="wget -qO"; else err "curl or wget is required."; fi

# --- Detect architecture -> Rust target triple used by the CI tarball name ---
_cpu="$(uname -m)"
case "$_cpu" in
  x86_64|amd64) _cpu="x86_64" ;;
  aarch64|arm64) _cpu="aarch64" ;;
  *) err "Unsupported CPU architecture: $_cpu" ;;
esac
_libc="gnu"
if ldd --version 2>&1 | grep -qi musl; then _libc="musl"; fi
TARGET="${_cpu}-unknown-linux-${_libc}"
TARBALL="stalwart-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${TARBALL}"

say "⏳ Downloading ${TARBALL} from ${REPO} (${RELEASE_TAG})..."
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
# shellcheck disable=SC2086
$DL "${TMP}/${TARBALL}" "$URL" || err "Download failed: $URL"
tar xzf "${TMP}/${TARBALL}" -C "$TMP" || err "Failed to extract ${TARBALL}."
[ -f "${TMP}/stalwart" ] || err "Binary 'stalwart' not found in tarball."

# --- Service account ---
if ! id "$ACCOUNT" >/dev/null 2>&1; then
  say "👤 Creating service account '${ACCOUNT}'..."
  useradd --system --no-create-home --shell /usr/sbin/nologin "$ACCOUNT" 2>/dev/null \
    || adduser --system --no-create-home --shell /usr/sbin/nologin "$ACCOUNT" 2>/dev/null \
    || err "Could not create the '${ACCOUNT}' user."
fi

# --- Directories ---
mkdir -p "$BIN_DIR" "$CONF_DIR" "$DATA_DIR" "$LOG_DIR"

# --- Install binary ---
say "📦 Installing binary to ${BIN_DIR}/stalwart..."
install -m 0755 "${TMP}/stalwart" "${BIN_DIR}/stalwart"

# --- Env file (recovery admin so you can complete setup at /admin) ---
ENV_FILE="${CONF_DIR}/stalwart.env"
if [ ! -e "$ENV_FILE" ]; then
  _pw="$(head -c 18 /dev/urandom | od -An -tx1 | tr -d ' \n' | cut -c1-24)"
  cat > "$ENV_FILE" <<EOF
STALWART_HOSTNAME=$(hostname -f 2>/dev/null || hostname)
# Temporary bootstrap admin for the initial /admin setup (change/remove after setup):
STALWART_RECOVERY_ADMIN=admin:${_pw}
EOF
  say "🔑 Wrote ${ENV_FILE} (bootstrap admin: admin / ${_pw})"
fi

# --- Ownership ---
chown -R "${ACCOUNT}:${ACCOUNT}" "$CONF_DIR" "$DATA_DIR" "$LOG_DIR"
chmod 0750 "$CONF_DIR" "$DATA_DIR" "$LOG_DIR"
chmod 0640 "$ENV_FILE"

# --- systemd service ---
if have systemctl; then
  say "🧩 Installing systemd unit..."
  cat > /etc/systemd/system/stalwart.service <<EOF
[Unit]
Description=Stalwart Mail Server (free edition)
After=network.target

[Service]
Type=simple
User=${ACCOUNT}
Group=${ACCOUNT}
EnvironmentFile=${ENV_FILE}
ExecStart=${BIN_DIR}/stalwart --config ${CONFIG_FILE}
Restart=on-failure
RestartSec=5
LimitNOFILE=65536
AmbientCapabilities=CAP_NET_BIND_SERVICE
NoNewPrivileges=true
ProtectSystem=full
ReadWritePaths=${DATA_DIR} ${LOG_DIR} ${CONF_DIR}

[Install]
WantedBy=multi-user.target
EOF
  systemctl daemon-reload
  systemctl enable --now stalwart.service
  say ""
  say "✅ Stalwart free edition is installed and running."
  say "   • No config yet -> it boots in bootstrap mode on port 8080."
  say "   • Finish setup at:  http://<server-ip>:8080/admin"
  say "   • Bootstrap admin is in: ${ENV_FILE}"
  say "   • Logs:  journalctl -u stalwart -f"
else
  say "⚠️  systemd not found. Start manually:"
  say "   ${BIN_DIR}/stalwart --config ${CONFIG_FILE}"
fi
