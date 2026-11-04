#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# e2e_install_test.sh — end-to-end test for scripts/install.sh (CMS edition).
#
# Fully userland (no root, no systemd): exercises the paths a real host takes.
#
#   Phase 1  install from a file:// prebuilt mirror (sha256-verified fast path,
#            no Rust toolchain needed), nohup service mode (Docker/WSL path),
#            real MySQL-wire client check.
#   Phase 2  tampered mirror (checksum mismatch) — the installer must refuse
#            the download.
#   Phase 3  re-run over an existing install — idempotent (binary kept,
#            service detected as already running).
#   Phase 4  --uninstall — binary + unit/pidfile removed, process stopped,
#            data directory kept.
#
# Env overrides: E2E_ROOT (work dir), E2E_PORT, PHP_BIN (mysqli client for the
# wire check; the phase is skipped when PHP with mysqli is unavailable).
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/.." && pwd)
MIRROR="$REPO/prebuilt/linux-x86_64"

T="${E2E_ROOT:-/tmp/dna-install-e2e}"
PREFIX="$T/usr"
DATA="$T/var/lib/dna-db"
PORT="${E2E_PORT:-13401}"
BIN="$PREFIX/bin/dna-db-core"

pass() { echo "[PASS] $*"; }
fail() { echo "[FAIL] $*"; exit 1; }
note() { echo "[NOTE] $*"; }

# --- 0) fixtures -----------------------------------------------------------
# Kill orphans from a previous E2E run (they hold the port; the pidfile they
# used was inside the work dir removed below).
pkill -f "dna-install-e2e" 2>/dev/null || true
sleep 1
[[ -x "$MIRROR/dna-db-core" ]] || fail "prebuilt binary missing in $MIRROR"
[[ -f "$MIRROR/SHA256SUMS" ]]  || fail "SHA256SUMS missing in $MIRROR"
rm -rf "$T"
mkdir -p "$T" "$T/tampered"

PHP_BIN=""
if [[ -n "${PHP_BIN_OVERRIDE:-}" ]]; then
    PHP_BIN="$PHP_BIN_OVERRIDE"
elif command -v php >/dev/null 2>&1; then
    PHP_BIN="php"
fi
if [[ -n "$PHP_BIN" ]] && ! "$PHP_BIN" -m 2>/dev/null | grep -qi '^mysqli$'; then
    note "$(basename "$PHP_BIN") has no mysqli — wire check will be skipped"
    PHP_BIN=""
fi

# --- phase 1: happy path ---------------------------------------------------
echo "== phase 1: install from file:// mirror (sha256 + nohup mode)"
PREBUILT_URL_BASE="file://$MIRROR" PREFIX="$PREFIX" \
    bash "$HERE/install.sh" --data-dir "$DATA" --port "$PORT" --user root:e2epass \
    > "$T/install.log" 2>&1 \
    || { cat "$T/install.log"; fail "install.sh exited non-zero"; }

grep -q "sha256 OK" "$T/install.log" \
    || { cat "$T/install.log"; fail "sha256-verified fast-path message not found"; }
pass "prebuilt fast-path accepted (checksum verified, no Rust toolchain)"

[[ -x "$BIN" ]] || fail "binary not installed at $BIN"
"$BIN" --version >/dev/null 2>&1 || fail "installed binary does not run"
pass "binary installed and runnable: $($BIN --version 2>/dev/null | head -1)"

[[ -f "$DATA/dna-db.pid" ]] || fail "pidfile missing — nohup mode not used"
PID=$(cat "$DATA/dna-db.pid")
kill -0 "$PID" 2>/dev/null || fail "server process (pid $PID) is not running"
pass "nohup service running (pid $PID, port $PORT, logs $DATA/dna-db.log)"

if [[ -n "$PHP_BIN" ]]; then
    "$PHP_BIN" -r '
        $m = @new mysqli("127.0.0.1", "root", "e2epass", "", (int)$argv[1]);
        if ($m->connect_errno) { fwrite(STDERR, "connect: ".$m->connect_error."\n"); exit(1); }
        $v = $m->query("SELECT VERSION() AS v")->fetch_assoc()["v"] ?? "";
        if ($v === "") { fwrite(STDERR, "VERSION() empty\n"); exit(1); }
        echo "wire ok: VERSION()=$v\n";
    ' "$PORT" > "$T/wire.log" 2>&1 \
        || { cat "$T/wire.log"; fail "mysqli wire check against the installed service"; }
    pass "$(cat "$T/wire.log")"
else
    note "wire check skipped (no PHP with mysqli found)"
fi

# --- phase 2: tampered mirror must be refused ------------------------------
echo "== phase 2: tampered mirror (checksum mismatch)"
mkdir -p "$T/tampered"
cp "$MIRROR/dna-db-core" "$T/tampered/"
printf 'x' >> "$T/tampered/dna-db-core"          # flip the payload
cp "$MIRROR/SHA256SUMS" "$T/tampered/"
TPREFIX="$T/usr-tampered"
PREBUILT_URL_BASE="file://$T/tampered" PREFIX="$TPREFIX" \
    bash "$HERE/install.sh" --data-dir "$T/var-tampered" --port "$PORT" \
    > "$T/tampered.log" 2>&1 && RC=0 || RC=$?
grep -q "checksum mismatch" "$T/tampered.log" \
    || { cat "$T/tampered.log"; fail "installer did not report a checksum mismatch"; }
if [[ $RC -eq 0 ]] && [[ -x "$TPREFIX/bin/dna-db-core" ]]; then
    # fallback to source build succeeded (cargo present) — the refusal still
    # happened, so the security property held; the binary is freshly built.
    pass "tampered download refused; installer fell back to a source build"
else
    [[ -x "$TPREFIX/bin/dna-db-core" ]] && fail "tampered binary was installed!"
    pass "tampered download refused (installer exited $RC without installing)"
fi

# --- phase 3: idempotent re-run --------------------------------------------
echo "== phase 3: re-run over the existing install"
PREBUILT_URL_BASE="file://$MIRROR" PREFIX="$PREFIX" \
    bash "$HERE/install.sh" --data-dir "$DATA" --port "$PORT" --user root:e2epass \
    > "$T/reinstall.log" 2>&1 \
    || { cat "$T/reinstall.log"; fail "second install.sh run failed"; }
grep -q "already running" "$T/reinstall.log" \
    || { cat "$T/reinstall.log"; fail "running service not detected on re-install"; }
grep -q "existing binary found" "$T/reinstall.log" \
    || { cat "$T/reinstall.log"; fail "existing binary not detected on re-install"; }
pass "re-install detected binary + running service (no duplicate daemon)"
PID2=$(cat "$DATA/dna-db.pid")
[[ "$PID2" == "$PID" ]] || fail "pidfile changed across re-install ($PID -> $PID2)"
pass "same daemon kept (pid $PID2)"

# --- phase 4: uninstall ----------------------------------------------------
echo "== phase 4: --uninstall"
PREFIX="$PREFIX" bash "$HERE/install.sh" --uninstall --data-dir "$DATA" \
    > "$T/uninstall.log" 2>&1 \
    || { cat "$T/uninstall.log"; fail "uninstall exited non-zero"; }
kill -0 "$PID" 2>/dev/null && fail "server process still alive after uninstall"
[[ -x "$BIN" ]] && fail "binary still present after uninstall"
[[ -f "$DATA/dna-db.pid" ]] && fail "pidfile still present after uninstall"
[[ -d "$DATA" ]] || fail "data directory was removed (must be kept)"
pass "service stopped, binary + pidfile removed, data dir $DATA kept"

echo
echo "E2E_INSTALL_OK"
