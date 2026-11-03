#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# install.sh — DNA-DB CMS edition ("easier than MySQL").
#
#   curl -fsSL https://raw.githubusercontent.com/nsmykh70-creator/dna-db-core/cms-edition/scripts/install.sh | sudo bash -s -- --wordpress
#
# What it does (no mysql_install_db, no mysql_secure_installation, no
# interactive root-password prompts, no CREATE DATABASE/GRANT):
#   1. installs the dna-db-core binary (cms feature included):
#      a sha256-verified prebuilt x86_64 download — no Rust toolchain
#      needed — falling back to a cargo build from source,
#   2. puts it in $PREFIX/bin/dna-db-core,
#   3. creates the data dir + a systemd unit listening on 3306
#      (no systemd — Docker/WSL/containers: nohup + pidfile instead),
#   4. starts the service — ready for WordPress/WooCommerce/Joomla DB_HOST.
#
# Flags:
#   --wordpress        bootstrap the 12 WordPress core tables at start
#   --user user:pass   MySQL login accepted by the wire listener
#                      (default: root with empty password + warning)
#   --port N           listener port (default 3306)
#   --data-dir PATH    data directory (default /var/lib/dna-db)
#   --uninstall        stop the service, remove unit + binary (data kept)
# Env:
#   PREFIX             install prefix (default /usr/local)
#   PREBUILT_URL_BASE  prebuilt mirror override (air-gapped hosts, forks)
set -euo pipefail

PORT=3306
DATA_DIR=/var/lib/dna-db
WP_FLAG=""
USER_FLAG=""
UNINSTALL=0
PREFIX="${PREFIX:-/usr/local}"
BIN="$PREFIX/bin/dna-db-core"
UNIT=/etc/systemd/system/dna-db-mysqld.service
PIDFILE=""
RAW_BASE="${PREBUILT_URL_BASE:-https://raw.githubusercontent.com/nsmykh70-creator/dna-db-core/cms-edition/prebuilt/linux-x86_64}"
REPO_URL="https://github.com/nsmykh70-creator/dna-db-core.git"
REPO_BRANCH="cms-edition"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --wordpress) WP_FLAG="--wordpress"; shift ;;
        --user) USER_FLAG="--user $2"; shift 2 ;;
        --port) PORT="$2"; shift 2 ;;
        --data-dir) DATA_DIR="$2"; shift 2 ;;
        --uninstall) UNINSTALL=1; shift ;;
        *) echo "unknown flag: $1" >&2; exit 2 ;;
    esac
done
PIDFILE="$DATA_DIR/dna-db.pid"

log() { echo "[install] $*"; }
have() { command -v "$1" >/dev/null 2>&1; }
# systemd is actually reachable (not just installed — Docker/WSL often have
# the binary but no init running there)
systemd_ok() { have systemctl && [[ -d /run/systemd/system ]]; }

fetch() { # fetch <url> <dest>
    if have curl; then curl -fsSL "$1" -o "$2"; return 0
    elif have wget; then wget -qO "$2" "$1"; return 0
    else return 127; fi
}

try_prebuilt() {
    [[ "$(uname -m)" == "x86_64" ]] || return 1
    local tmp; tmp=$(mktemp -d)
    fetch "$RAW_BASE/dna-db-core" "$tmp/dna-db-core" || { rm -rf "$tmp"; return 1; }
    fetch "$RAW_BASE/SHA256SUMS" "$tmp/SHA256SUMS" || { rm -rf "$tmp"; return 1; }
    (cd "$tmp" && sha256sum -c --quiet SHA256SUMS) || {
        echo "[install]   checksum mismatch — refusing the download" >&2
        rm -rf "$tmp"; return 1
    }
    install -m 0755 "$tmp/dna-db-core" "$BIN"
    rm -rf "$tmp"
}

build_from_source() {
    if ! have cargo; then
        echo "[install]   prebuilt unavailable and cargo not found." >&2
        echo "[install]   install Rust first: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
        return 1
    fi
    local work; work=$(mktemp -d)
    git clone --depth 1 --branch "$REPO_BRANCH" "$REPO_URL" "$work/src" >&2
    (cd "$work/src" && cargo build --release --features cms)
    install -m 0755 "$work/src/target/release/dna-db-core" "$BIN"
    rm -rf "$work"
}

do_uninstall() {
    log "uninstall"
    if systemd_ok; then
        systemctl disable --now dna-db-mysqld.service 2>/dev/null || true
        rm -f "$UNIT"
        systemctl daemon-reload 2>/dev/null || true
    fi
    if [[ -f "$PIDFILE" ]]; then
        kill "$(cat "$PIDFILE")" 2>/dev/null || true
        rm -f "$PIDFILE"
    fi
    rm -f "$BIN"
    log "removed $UNIT (if present) and $BIN"
    log "data dir $DATA_DIR kept — remove manually if no longer needed"
}

if [[ $UNINSTALL -eq 1 ]]; then
    if systemd_ok && [[ $EUID -ne 0 ]]; then
        echo "[install] needs root (systemd + $BIN); re-run with sudo" >&2
        exit 1
    fi
    do_uninstall
    exit 0
fi

if systemd_ok && [[ $EUID -ne 0 ]]; then
    echo "[install] needs root (systemd + $BIN); re-run with sudo" >&2
    exit 1
fi

log "1/4 binary"
mkdir -p "$(dirname "$BIN")" 2>/dev/null || true
if [[ -x "$BIN" ]] && "$BIN" --version >/dev/null 2>&1; then
    log "  existing binary found: $BIN ($("$BIN" --version 2>/dev/null | head -1))"
else
    if try_prebuilt; then
        log "  prebuilt binary installed (sha256 OK) — no Rust needed"
    elif build_from_source; then
        log "  built from source"
    else
        echo "[install]   could not fetch the prebuilt binary or build from source" >&2
        exit 1
    fi
fi

log "2/4 data directory $DATA_DIR"
mkdir -p "$DATA_DIR" || { echo "[install] cannot write $DATA_DIR — pass --data-dir" >&2; exit 1; }
# dna-db locks its data dir itself; no manual init step exists (unlike MySQL).

log "3/4 service"
START_ARGS=(mysqld --data-dir "$DATA_DIR" --bind 0.0.0.0 --port "$PORT")
[[ -n "$WP_FLAG" ]] && START_ARGS+=(--wordpress)
[[ -n "$USER_FLAG" ]] && START_ARGS+=(--user "${USER_FLAG#--user }")
# shell-quoted ExecStart line — safe for passwords with spaces/specials
QEXEC=$(printf ' %q' "${START_ARGS[@]}")

MODE=systemd
if systemd_ok; then
    cat > "$UNIT" <<UNIT
[Unit]
Description=DNA-DB MySQL-compatible server (CMS edition)
After=network.target

[Service]
ExecStart=$BIN$QEXEC
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
else
    MODE=nohup
fi

log "4/4 start"
if [[ $MODE == systemd ]]; then
    systemctl enable --now dna-db-mysqld.service
    for _ in $(seq 1 30); do
        systemctl is-active --quiet dna-db-mysqld.service && break
        sleep 0.5
    done
    systemctl --no-pager --lines 5 status dna-db-mysqld.service || true
else
    if [[ -f "$PIDFILE" ]] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
        log "  already running (pid $(cat "$PIDFILE"))"
    else
        # Fully detached stdio (stdin too — a daemon holding the caller's
        # pipe open hangs non-interactive shells), but NO setsid: `$!` must
        # be the daemon's real pid so `kill $(cat pidfile)` stops it.
        nohup "$BIN" "${START_ARGS[@]}" >>"$DATA_DIR/dna-db.log" 2>&1 </dev/null &
        disown 2>/dev/null || true
        echo $! > "$PIDFILE"
    fi
    for _ in $(seq 1 30); do
        (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && break
        sleep 0.5
    done
    log "  pid $(cat "$PIDFILE"), logs: $DATA_DIR/dna-db.log"
fi

HOST=127.0.0.1
DB_USER="root"
DB_PASS_HINT="empty (trust mode — pass --user user:pass for production)"
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

EOF

if [[ $MODE == systemd ]]; then
    cat <<EOF
Check:  systemctl status dna-db-mysqld
Stop:   systemctl stop dna-db-mysqld
Logs:   journalctl -u dna-db-mysqld -f
EOF
else
    cat <<EOF
Check:  kill -0 \$(cat $PIDFILE)
Stop:   kill \$(cat $PIDFILE)
Logs:   tail -f $DATA_DIR/dna-db.log
EOF
fi
