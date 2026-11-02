#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# install.sh — DNA-DB CMS edition ("easier than MySQL").
#
#   curl -fsSL https://raw.githubusercontent.com/nsmykh70-creator/dna-db-core/cms-edition/scripts/install.sh | sudo bash -s -- --wordpress
#
# What it does (no mysql_install_db, no mysql_secure_installation, no
# interactive root-password prompts, no CREATE DATABASE/GRANT):
#   1. builds (or fetches) the dna-db-core binary with the cms feature,
#   2. installs /usr/local/bin/dna-db-core,
#   3. creates /var/lib/dna-db (data) + a systemd unit listening on 3306,
#   4. starts the service — ready for WordPress/WooCommerce/Joomla DB_HOST.
#
# Flags:
#   --wordpress        bootstrap the 12 WordPress core tables at start
#   --user user:pass   MySQL login accepted by the wire listener
#                      (default: root with empty password + warning)
#   --port N           listener port (default 3306)
#   --data-dir PATH    data directory (default /var/lib/dna-db)
set -euo pipefail

PORT=3306
DATA_DIR=/var/lib/dna-db
WP_FLAG=""
USER_FLAG=""
REPO_URL="https://github.com/nsmykh70-creator/dna-db-core.git"
REPO_BRANCH="cms-edition"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --wordpress) WP_FLAG="--wordpress"; shift ;;
        --user) USER_FLAG="--user $2"; shift 2 ;;
        --port) PORT="$2"; shift 2 ;;
        --data-dir) DATA_DIR="$2"; shift 2 ;;
        *) echo "unknown flag: $1" >&2; exit 2 ;;
    esac
done

if [[ $EUID -ne 0 ]]; then
    echo "[install] needs root (systemd + /usr/local/bin); re-run with sudo" >&2
    exit 1
fi

echo "[install] 1/4 binary"
BIN=/usr/local/bin/dna-db-core
if [[ -x "$BIN" ]] && "$BIN" --version >/dev/null 2>&1; then
    echo "[install]   existing binary found: $BIN"
else
    WORK=$(mktemp -d)
    if command -v cargo >/dev/null 2>&1; then
        echo "[install]   building from source (cargo)"
        git clone --depth 1 --branch "$REPO_BRANCH" "$REPO_URL" "$WORK/src"
        (cd "$WORK/src" && cargo build --release --features cms)
        install -m 0755 "$WORK/src/target/release/dna-db-core" "$BIN"
    else
        echo "[install]   cargo not found — install Rust first:" >&2
        echo "             curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
        exit 1
    fi
    rm -rf "$WORK"
fi

echo "[install] 2/4 data directory $DATA_DIR"
mkdir -p "$DATA_DIR"
# dna-db locks its data dir itself; no manual init step exists (unlike MySQL).

echo "[install] 3/4 systemd unit"
cat >/etc/systemd/system/dna-db-mysqld.service <<UNIT
[Unit]
Description=DNA-DB MySQL-compatible server (CMS edition)
After=network.target

[Service]
ExecStart=$BIN mysqld --data-dir $DATA_DIR --bind 0.0.0.0 --port $PORT $WP_FLAG $USER_FLAG
Restart=on-failure
RestartSec=2
# The wire listener speaks plaintext MySQL protocol; put TLS termination
# (stunnel/envoy) in front for untrusted networks — same guidance as pgwire.
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=$DATA_DIR

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload

echo "[install] 4/4 start"
systemctl enable --now dna-db-mysqld.service
sleep 1
systemctl --no-pager --lines 5 status dna-db-mysqld.service || true

HOST=$(systemctl show -p ExecStart --value dna-db-mysqld.service | grep -o '127.0.0.1' >/dev/null && echo 127.0.0.1 || echo 127.0.0.1)
DB_USER="root"
DB_PASS_HINT="anything (trust mode)"
if [[ -n "$USER_FLAG" ]]; then
    DB_USER="${USER_FLAG#--user }"
    DB_USER="${DB_USER%%:*}"
    DB_PASS_HINT="the password from --user"
fi

cat <<EOF

[install] DONE — MySQL-compatible server on :$PORT

wp-config.php:
    define( 'DB_NAME', 'wordpress' );
    define( 'DB_USER', '$DB_USER' );
    define( 'DB_PASSWORD', '$DB_PASS_HINT' );
    define( 'DB_HOST', '$HOST:$PORT' );

or (PDO, Joomla configuration.php):
    host: $HOST   port: $PORT   user: $DB_USER

Check:  systemctl status dna-db-mysqld
Stop:   systemctl stop dna-db-mysqld
Logs:   journalctl -u dna-db-mysqld -f
EOF
