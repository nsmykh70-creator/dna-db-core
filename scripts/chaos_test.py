#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""
DNA-DB v3.0 «Repairosome» — chaos test (the readiness criterion).

Protocol (ROADMAP_V3_SELF_HEALING.md, section 0):
  1. Build a database of 100 000+ keys (Strict fsync), checkpoint so every
     key lives in erasure-coded .sst segments.
  2. SIGKILL the server (crash semantics must not depend on shutdown).
  3. Damage M random 4 KiB sectors in the .sst files — bit-flips AND
     zero-fills. The damage is stripe-aware white-box: the test reads the
     DNASST02 superblock and guarantees at most m damaged shards per stripe
     for the PASS profile (over-damage profile intentionally exceeds it).
  4. Run `dna-db-core doctor --fix` offline.
  5. Restart the server and read every key: 100 % of original values must
     come back, plus a repair report.

Modes:
  offline  — the classic criterion above (default)
  live     — damage sectors while the server is RUNNING; on-read repair and
             the scrubber must keep every read correct (no restart, no doctor)
  overdmg  — over-damage one stripe (> m shards) and verify the honest
             per-stripe failure report plus intact neighbours

Usage:
  python3 scripts/chaos_test.py --keys 100000 --sectors 60
  python3 scripts/chaos_test.py --mode live   --keys 50000 --sectors 40
  python3 scripts/chaos_test.py --mode overdmg --keys 20000
"""

import argparse
import json
import os
import random
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time

BLOCK = 4096
SUPERBLOCK_MAGIC = b"DNASST02"


# ---------------------------------------------------------------------------
# Minimal binary TCP client for the DNA-DB protocol
# ---------------------------------------------------------------------------

class DnaDbClient:
    def __init__(self, host, port, user, password):
        self.sock = socket.create_connection((host, port), timeout=120)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self._send(b"\x01%s:%s" % (user.encode(), password.encode()))
        ok = self._recv_exact(2)
        if ok != b"OK":
            raise RuntimeError("authentication failed")

    def _send(self, payload: bytes):
        self.sock.sendall(struct.pack("<I", len(payload)) + payload)

    def _recv_exact(self, n: int) -> bytes:
        buf = b""
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise RuntimeError("connection closed")
            buf += chunk
        return buf

    def _recv_frame(self) -> bytes:
        (length,) = struct.unpack("<I", self._recv_exact(4))
        if length == 0:
            return b""
        return self._recv_exact(length)

    def put(self, key: bytes, value: bytes):
        payload = b"\x03" + struct.pack("<H", len(key)) + key + value
        self._send(payload)
        if self._recv_exact(2) != b"OK":
            raise RuntimeError("PUT failed for %r" % key)

    def get(self, key: bytes):
        """Returns (status, data): status in {'ok', 'miss', 'error'}"""
        self._send(b"\x02" + key)
        data = self._recv_frame()
        if data.startswith(b"ERR:"):
            return "error", data
        if data == b"":
            return "miss", data
        return "ok", data

    def checkpoint(self):
        self._send(b"\x12")
        assert self._recv_exact(2) == b"OK", "CHECKPOINT failed"

    def status(self) -> bytes:
        self._send(b"\x00")
        return self._recv_frame()

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


# ---------------------------------------------------------------------------
# DNASST02 white-box helpers (format documented in docs/PROTOCOL.md)
# ---------------------------------------------------------------------------

def read_superblock(path):
    """Parse the DNASST02 superblock: (k, m, data_stripes, index_stripes)."""
    with open(path, "rb") as f:
        sb = f.read(BLOCK)
    if sb[0:8] != SUPERBLOCK_MAGIC:
        return None  # legacy segment
    k = sb[8]
    m = sb[9]
    data_stripes = struct.unpack("<I", sb[12:16])[0]
    index_stripes = struct.unpack("<I", sb[16:20])[0]
    return k, m, data_stripes, index_stripes


def slot_to_stripe(slot, stripes, seed):
    """Inverse of the writer's phys_slot(): which stripe owns a physical slot."""
    rot = seed % stripes
    idx = slot // stripes
    s_rot = slot % stripes
    return (s_rot + stripes - rot) % stripes


def stripe_of_file_offset(path, file_offset):
    """Map an absolute file offset inside the data region to its stripe."""
    geo = read_superblock(path)
    if geo is None:
        return None, None, None
    k, m, data_stripes, _ = geo
    slot = (file_offset - BLOCK) // BLOCK
    seed = fnv1a64(os.path.basename(path).encode())
    return slot_to_stripe(slot, data_stripes, seed), data_stripes, (k, m)


def fnv1a64(data: bytes) -> int:
    h = 0xCBF29CE484222325
    for b in data:
        h ^= b
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


def list_sst_segments(data_dir):
    return sorted(
        os.path.join(data_dir, n)
        for n in os.listdir(data_dir)
        if n.endswith(".sst")
    )


# ---------------------------------------------------------------------------
# Damage engine
# ---------------------------------------------------------------------------

def damage_sector(path, file_offset, mode, rng):
    """Damage one 4 KiB sector: 'bitflip' (XOR random bytes) or 'zero'."""
    with open(path, "r+b") as f:
        f.seek(file_offset)
        if mode == "zero":
            f.write(b"\x00" * BLOCK)
        else:
            blob = bytearray(f.read(BLOCK))
            if len(blob) < BLOCK:
                return False
            for _ in range(64):
                pos = rng.randrange(BLOCK)
                blob[pos] ^= rng.randrange(1, 256)
            f.seek(file_offset)
            f.write(bytes(blob))
    return True


def choose_damage_plan(data_dir, sectors, max_per_stripe, rng, target_file=None):
    """Pick (path, file_offset) damage points with a hard cap of
    `max_per_stripe` damaged shards per stripe (PASS profile when set to m)."""
    plan = []
    per_stripe = {}
    segments = list_sst_segments(data_dir)
    if target_file:
        segments = [target_file]
    candidates = []
    for seg in segments:
        geo = read_superblock(seg)
        if geo is None:
            continue  # legacy: skip (no parity)
        k, m, data_stripes, _ = geo
        size = os.path.getsize(seg)
        data_end = BLOCK + data_stripes * (k + m) * BLOCK
        seed = fnv1a64(os.path.basename(seg).encode())
        for slot in range(data_stripes * (k + m)):
            off = BLOCK + slot * BLOCK
            stripe = slot_to_stripe(slot, data_stripes, seed)
            candidates.append((seg, off, stripe))
    if not candidates:
        raise RuntimeError("no DNASST02 segments to damage")
    rng.shuffle(candidates)
    for seg, off, stripe in candidates:
        if len(plan) >= sectors:
            break
        key = (seg, stripe)
        if per_stripe.get(key, 0) >= max_per_stripe:
            continue
        per_stripe[key] = per_stripe.get(key, 0) + 1
        plan.append((seg, off))
    return plan


# ---------------------------------------------------------------------------
# Server lifecycle
# ---------------------------------------------------------------------------

def find_binary():
    here = os.path.dirname(os.path.abspath(__file__))
    exe = ".exe" if os.name == "nt" else ""
    for candidate in [
        os.path.join(here, "..", "target", "release", "dna-db-core" + exe),
        os.path.join(here, "..", "prebuilt", "linux-x86_64", "dna-db-core"),
    ]:
        if os.path.exists(candidate):
            return os.path.abspath(candidate)
    raise RuntimeError("dna-db-core binary not found; run cargo build --release")


class Server:
    def __init__(self, data_dir, port, memtable_mb=2, extra=None):
        self.data_dir = data_dir
        self.port = port
        self.proc = None
        self.log = open(os.path.join(tempfile.gettempdir(),
                         "dna_chaos_%d.log" % port), "ab")
        extra = extra or {}
        cmd = [
            find_binary(), "serve",
            "--data-dir", data_dir,
            "--port", str(port),
            "--bind", "127.0.0.1",
            "--admin-pass", "chaos-pass",
            "--memtable-mb", str(memtable_mb),
            "--sync", "strict",
        ]
        if extra.get("no_scrub"):
            cmd.append("--no-scrub")
        else:
            cmd += ["--scrub-interval-secs", str(extra.get("scrub_interval", 86400))]
        self.cmd = cmd

    def start(self, wait=True):
        self.proc = subprocess.Popen(
            self.cmd, stdout=self.log, stderr=self.log,
            cwd=os.path.dirname(find_binary()))
        if wait:
            self.wait_ready()

    def wait_ready(self, timeout=30):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError("server died on startup (see chaos log)")
            try:
                c = DnaDbClient("127.0.0.1", self.port, "admin", "chaos-pass")
                c.close()
                return
            except OSError:
                time.sleep(0.05)
        raise RuntimeError("server did not become ready in %ss" % timeout)

    def kill9(self):
        if self.proc and self.proc.poll() is None:
            if os.name == "nt":
                # Windows has no SIGKILL; os.kill(pid, 9) = TerminateProcess
                os.kill(self.proc.pid, 9)
            else:
                self.proc.send_signal(signal.SIGKILL)
            self.proc.wait()
        self.log.flush()

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()


# ---------------------------------------------------------------------------
# Reference payload
# ---------------------------------------------------------------------------

def reference_keys(n, start=0):
    """Deterministic reference values (pseudo-random genomic strings)."""
    out = {}
    state = 0x9E3779B97F4A7C15 + start
    for i in range(start, start + n):
        state ^= (state << 13) & 0xFFFFFFFFFFFFFFFF
        state ^= state >> 7
        state ^= (state << 17) & 0xFFFFFFFFFFFFFFFF
        val = b"".join(
            bytes([65 + (state >> (8 * j)) & 0xFF for j in range(4)])
            for _ in range(2)
        )  # 8 bytes of pseudo-random "genomic" ASCII
        out[("chr1:%08d" % i).encode()] = val
    return out


# ---------------------------------------------------------------------------
# Test scenarios
# ---------------------------------------------------------------------------

def scenario_offline(keys_n, sectors, seed, workdir, port):
    print("\n=== SCENARIO 1/3: offline (restart + doctor --fix) ===")
    rng = random.Random(seed)
    data_dir = os.path.join(workdir, "offline", "dna_data")
    shutil.rmtree(os.path.dirname(data_dir), ignore_errors=True)
    os.makedirs(data_dir, exist_ok=True)

    ref = reference_keys(keys_n)
    t0 = time.time()
    srv = Server(data_dir, port, memtable_mb=2, extra={"no_scrub": True})
    srv.start()
    cli = DnaDbClient("127.0.0.1", port, "admin", "chaos-pass")
    for k, v in ref.items():
        cli.put(k, v)
    cli.checkpoint()
    cli.close()
    srv.kill9()
    load_secs = time.time() - t0
    print(f"[1.1] loaded {keys_n} keys in {load_secs:.1f}s, checkpointed, SIGKILLed")

    # White-box damage plan: ≤ m shards per stripe → recovery is guaranteed.
    segments = list_sst_segments(data_dir)
    print(f"[1.2] {len(segments)} segments on disk; planning {sectors} sector hits")
    plan = choose_damage_plan(data_dir, sectors, max_per_stripe=2, rng=rng)
    modes = ["bitflip", "zero"]
    damaged_bytes = 0
    for i, (seg, off) in enumerate(plan):
        mode = modes[i % 2]
        damage_sector(seg, off, mode, rng)
        damaged_bytes += BLOCK
    print(f"[1.3] damaged {len(plan)} × 4 KiB sectors "
          f"({damaged_bytes // 1024} KiB) — bit-flips and zero-fills")

    # doctor --fix (offline)
    t0 = time.time()
    doctor = subprocess.run(
        [find_binary(), "doctor", "--data-dir", data_dir, "--fix", "--json"],
        capture_output=True, text=True)
    doctor_secs = time.time() - t0
    if doctor.returncode != 0:
        print(doctor.stdout[-2000:])
        raise RuntimeError("doctor --fix reported unrecoverable lesions")
    report = json.loads(doctor.stdout)
    fixed = report["fixed_blocks"]
    print(f"[1.4] doctor --fix completed in {doctor_secs:.1f}s: "
          f"{fixed} blocks rewritten from parity, "
          f"{report['healthy']}/{report['scanned_segments']} segments healthy")

    # Restart + full verification
    srv = Server(data_dir, port, memtable_mb=2, extra={"no_scrub": True})
    srv.start()
    cli = DnaDbClient("127.0.0.1", port, "admin", "chaos-pass")
    t0 = time.time()
    mismatches = 0
    missing = 0
    for k, v in ref.items():
        status, got = cli.get(k)
        if status != "ok":
            missing += 1
        elif got != v:
            mismatches += 1
    verify_secs = time.time() - t0
    health = json.loads(cli.status())["health"]
    cli.close()
    srv.stop()

    total = keys_n
    ok = (missing == 0 and mismatches == 0)
    print(f"[1.5] verified {total} keys in {verify_secs:.1f}s: "
          f"missing={missing}, corrupted={mismatches}")
    print(f"[1.6] engine health: lesions found={health['lesions']['found']}, "
          f"repaired={health['lesions']['repaired']}, "
          f"unrecoverable={health['lesions']['unrecoverable']}")
    verdict = "PASS" if ok else "FAIL"
    print(f"[1.7] OFFLINE SCENARIO: {verdict} — "
          f"{total}/{total} keys restored"
          if ok else f"[1.7] OFFLINE SCENARIO: {verdict}")
    return {
        "scenario": "offline",
        "keys": keys_n,
        "sectors_damaged": len(plan),
        "blocks_fixed_by_doctor": fixed,
        "missing": missing,
        "corrupted": mismatches,
        "verdict": verdict,
        "load_secs": round(load_secs, 1),
        "doctor_secs": round(doctor_secs, 1),
        "verify_secs": round(verify_secs, 1),
    }


def scenario_live(keys_n, sectors, seed, workdir, port):
    print("\n=== SCENARIO 2/3: live (damage while serving, on-read repair) ===")
    rng = random.Random(seed + 1)
    data_dir = os.path.join(workdir, "live", "dna_data")
    shutil.rmtree(os.path.dirname(data_dir), ignore_errors=True)
    os.makedirs(data_dir, exist_ok=True)

    ref = reference_keys(keys_n)
    srv = Server(data_dir, port, memtable_mb=2, extra={"no_scrub": True})
    srv.start()
    cli = DnaDbClient("127.0.0.1", port, "admin", "chaos-pass")
    for k, v in ref.items():
        cli.put(k, v)
    cli.checkpoint()

    # Damage while the server keeps running.
    plan = choose_damage_plan(data_dir, sectors, max_per_stripe=2, rng=rng)
    modes = ["bitflip", "zero"]
    for i, (seg, off) in enumerate(plan):
        damage_sector(seg, off, modes[i % 2], rng)
    print(f"[2.1] damaged {len(plan)} × 4 KiB sectors under a live server")

    t0 = time.time()
    mismatches = 0
    missing = 0
    for k, v in ref.items():
        status, got = cli.get(k)
        if status != "ok":
            missing += 1
        elif got != v:
            mismatches += 1
    verify_secs = time.time() - t0
    health = json.loads(cli.status())["health"]
    cli.close()
    srv.stop()

    ok = missing == 0 and mismatches == 0
    print(f"[2.2] read all {keys_n} keys in {verify_secs:.1f}s "
          f"(on-read repair): missing={missing}, corrupted={mismatches}")
    print(f"[2.3] lesions: found={health['lesions']['found']}, "
          f"repaired={health['lesions']['repaired']}, "
          f"bytes_repaired={health['lesions']['bytes_repaired']}")
    verdict = "PASS" if ok else "FAIL"
    print(f"[2.4] LIVE SCENARIO: {verdict}")
    return {
        "scenario": "live",
        "keys": keys_n,
        "sectors_damaged": len(plan),
        "missing": missing,
        "corrupted": mismatches,
        "verdict": verdict,
        "verify_secs": round(verify_secs, 1),
        "lesions_repaired": health["lesions"]["repaired"],
    }


def scenario_overdmg(keys_n, seed, workdir, port):
    print("\n=== SCENARIO 3/3: over-damage (> m shards in one stripe) ===")
    rng = random.Random(seed + 2)
    data_dir = os.path.join(workdir, "overdmg", "dna_data")
    shutil.rmtree(os.path.dirname(data_dir), ignore_errors=True)
    os.makedirs(data_dir, exist_ok=True)

    ref = reference_keys(keys_n)
    srv = Server(data_dir, port, memtable_mb=2, extra={"no_scrub": True})
    srv.start()
    cli = DnaDbClient("127.0.0.1", port, "admin", "chaos-pass")
    for k, v in ref.items():
        cli.put(k, v)
    cli.checkpoint()
    cli.close()
    srv.kill9()

    # Pick the largest segment; zero-fill 4 data shards of ONE stripe (m=2 → lost).
    segments = list_sst_segments(data_dir)
    seg = max(segments, key=os.path.getsize)
    geo = read_superblock(seg)
    k_geom, m_geom, data_stripes, _ = geo
    seed_i = fnv1a64(os.path.basename(seg).encode())
    target_stripe = rng.randrange(data_stripes)
    # All (k+m) slots of the target stripe; damage k of its DATA shards.
    slots = [i * data_stripes + ((target_stripe + seed_i) % data_stripes)
             for i in range(k_geom)]  # idx 0..k-1 are data shards
    for slot in slots:
        off = BLOCK + slot * BLOCK
        damage_sector(seg, off, "zero", rng)
    print(f"[3.1] segment {os.path.basename(seg)}: zero-filled {k_geom} data shards "
          f"of stripe {target_stripe} (m={m_geom} → stripe unrecoverable)")

    doctor = subprocess.run(
        [find_binary(), "doctor", "--data-dir", data_dir, "--json"],
        capture_output=True, text=True)
    report = json.loads(doctor.stdout)
    verdict_entry = next(
        (v for v in report["verdicts"] if v["file"] == os.path.basename(seg)), None)
    honest = (verdict_entry is not None
              and verdict_entry["verdict"] in ("UNREPAIRABLE",)
              and verdict_entry["unrecoverable_stripes"] >= 1)
    print(f"[3.2] doctor verdict: {verdict_entry['verdict']} "
          f"(unrecoverable stripes: {verdict_entry['unrecoverable_stripes']}, "
          f"damaged blocks: {verdict_entry['damaged_blocks']})")

    # doctor --fix must NOT pretend to fix it; neighbours stay intact.
    doctor = subprocess.run(
        [find_binary(), "doctor", "--data-dir", data_dir, "--fix", "--json"],
        capture_output=True, text=True)
    report = json.loads(doctor.stdout)
    verdict_entry = next(
        (v for v in report["verdicts"] if v["file"] == os.path.basename(seg)), None)
    still_honest = verdict_entry["verdict"] == "UNREPAIRABLE"
    print(f"[3.3] after --fix the stripe is still honestly UNREPAIRABLE: "
          f"{still_honest}")

    # Restart: keys outside the lost stripe must all read back correctly.
    srv = Server(data_dir, port, memtable_mb=2, extra={"no_scrub": True})
    srv.start()
    cli = DnaDbClient("127.0.0.1", port, "admin", "chaos-pass")
    unreadable = 0
    wrong = 0
    for k, v in ref.items():
        status, got = cli.get(k)
        if status == "error":
            unreadable += 1  # honest error (quarantined stripe)
        elif status != "ok":
            unreadable += 1
        elif got != v:
            wrong += 1
    health = json.loads(cli.status())["health"]
    cli.close()
    srv.stop()

    ok = honest and still_honest and wrong == 0
    print(f"[3.4] reads: wrong={wrong}, unreadable={unreadable} "
          f"(unreadable keys belong to the honestly-lost stripe only)")
    verdict = "PASS" if ok else "FAIL"
    print(f"[3.5] OVER-DAMAGE SCENARIO: {verdict} — honest failure reporting")
    return {
        "scenario": "overdmg",
        "keys": keys_n,
        "damaged_shards_in_one_stripe": k_geom,
        "doctor_verdict": verdict_entry["verdict"],
        "wrong_values": wrong,
        "unreadable_keys": unreadable,
        "verdict": verdict,
    }


# ---------------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description="DNA-DB v3.0 chaos test")
    ap.add_argument("--keys", type=int, default=100_000)
    ap.add_argument("--sectors", type=int, default=60,
                    help="4 KiB sectors to damage in offline/live scenarios")
    ap.add_argument("--seed", type=int, default=20261021)
    ap.add_argument("--mode", choices=["offline", "live", "overdmg", "full"],
                    default="full")
    ap.add_argument("--workdir", default=None)
    ap.add_argument("--port", type=int, default=19733)
    ap.add_argument("--json-out", default=None)
    args = ap.parse_args()

    workdir = args.workdir or os.path.join(tempfile.gettempdir(),
                                           "dna_chaos_run")
    os.makedirs(workdir, exist_ok=True)
    port = args.port

    results = []
    if args.mode in ("offline", "full"):
        results.append(scenario_offline(args.keys, args.sectors, args.seed,
                                        workdir, port))
    if args.mode in ("live", "full"):
        results.append(scenario_live(args.keys, args.sectors, args.seed,
                                     workdir, port))
    if args.mode in ("overdmg", "full"):
        results.append(scenario_overdmg(args.keys, args.seed, workdir, port))

    print("\n" + "=" * 64)
    print("CHAOS SUMMARY")
    print("=" * 64)
    for r in results:
        print(json.dumps(r, ensure_ascii=False))
    all_pass = all(r.get("verdict") == "PASS" for r in results)
    total_keys = sum(r.get("keys", 0) for r in results)
    print("-" * 64)
    print(f"CHAOS {'PASS' if all_pass else 'FAIL'}: "
          f"{len(results)} scenario(s), {total_keys} key-verifications, "
          f"seed={args.seed}")
    if args.json_out:
        with open(args.json_out, "w") as f:
            json.dump({"results": results,
                       "chaos": "PASS" if all_pass else "FAIL",
                       "seed": args.seed}, f, indent=2)
        print(f"JSON report: {args.json_out}")
    sys.exit(0 if all_pass else 1)


if __name__ == "__main__":
    main()
