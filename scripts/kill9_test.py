#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""
DNA-DB Core Crash Durability Verification (kill -9)
Simulates sudden SIGKILL terminations during intensive concurrent writes
and asserts 100% recovery of all acknowledged transactions.
"""

import os
import sys
import time
import socket
import signal
import shutil
import subprocess
import threading

SERVER_BIN = "./target/release/dna-db-core" + (".exe" if os.name == "nt" else "")
SERVER_PORT = 9999
DATA_DIR = "./target/dna_test_data"
# Windows has no SIGKILL; os.kill(pid, 9) maps to TerminateProcess —
# same semantics: immediate death, no cleanup, no graceful shutdown.
KILL_SIG = getattr(signal, "SIGKILL", 9)

acknowledged_writes = {}
lock = threading.Lock()
stop_flag = False

def readn(sock, n):
    """Read exactly n bytes (recv() may return short frames)."""
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("EOF")
        buf += chunk
    return buf

def client_worker(worker_id):
    global acknowledged_writes
    try:
        s = socket.create_connection(("127.0.0.1", SERVER_PORT), timeout=2)
        # Auth: admin:password
        auth_msg = b"\x01admin:password"
        s.sendall(len(auth_msg).to_bytes(4, 'little') + auth_msg)
        readn(s, 2) # OK

        seq = 0
        while not stop_flag:
            key = f"key_{worker_id}_{seq}".encode()
            val = f"ACTGACTGACTGACTG_{worker_id}_{seq}".encode()
            
            # PUT opcode = 0x03
            payload = bytearray([0x03])
            payload.extend(len(key).to_bytes(2, 'little'))
            payload.extend(key)
            payload.extend(val)

            s.sendall(len(payload).to_bytes(4, 'little') + payload)
            resp = readn(s, 2)
            if resp == b"OK":
                with lock:
                    acknowledged_writes[key] = val
            seq += 1
    except Exception:
        pass

def main():
    print("[TEST] Starting DNA-DB kill -9 crash durability test...")
    # Fresh datadir: leftover data from a previous run could mask losses
    # (keys/values are deterministic).
    shutil.rmtree(DATA_DIR, ignore_errors=True)
    os.makedirs(DATA_DIR, exist_ok=True)

    # 1. Start server
    proc = subprocess.Popen([SERVER_BIN, "serve", "--data-dir", DATA_DIR, "--port", str(SERVER_PORT), "--admin-pass", "password"])
    time.sleep(1)

    # 2. Launch 8 concurrent write threads
    threads = [threading.Thread(target=client_worker, args=(i,)) for i in range(8)]
    for t in threads:
        t.start()

    time.sleep(3) # Let write load build up

    # 3. Brutally kill with SIGKILL (kill -9)
    print(f"[TEST] Sending SIGKILL (kill -9) to PID {proc.pid}...")
    global stop_flag
    stop_flag = True
    os.kill(proc.pid, KILL_SIG)
    proc.wait()

    print(f"[TEST] Total acknowledged writes before crash: {len(acknowledged_writes)}")

    # 4. Restart server and verify WAL replay
    print("[TEST] Restarting DNA-DB server to trigger WAL replay and torn-write auto-recovery...")
    proc2 = subprocess.Popen([SERVER_BIN, "serve", "--data-dir", DATA_DIR, "--port", str(SERVER_PORT), "--admin-pass", "password"])
    time.sleep(2)

    try:
        s = socket.create_connection(("127.0.0.1", SERVER_PORT), timeout=2)
        s.sendall(len(b"\x01admin:password").to_bytes(4, 'little') + b"\x01admin:password")
        readn(s, 2)

        missing = 0
        corrupt = 0
        for key, val in acknowledged_writes.items():
            payload = bytearray([0x02]) + key
            s.sendall(len(payload).to_bytes(4, 'little') + payload)
            resp_len = int.from_bytes(readn(s, 4), 'little')
            resp_val = readn(s, resp_len) if resp_len else b""
            if not resp_val:
                missing += 1
            elif resp_val != val:
                corrupt += 1

        print(f"[RESULT] Verification complete:")
        print(f"  - Total checked: {len(acknowledged_writes)}")
        print(f"  - Missing: {missing}")
        print(f"  - Corrupt: {corrupt}")

        if missing == 0 and corrupt == 0:
            print(">>> SUCCESS: 100% DURABILITY VERIFIED UNDER SIGKILL (kill -9) <<<")
        else:
            print(">>> FAILURE: Data loss detected! <<<")
            sys.exit(1)
    finally:
        proc2.terminate()

if __name__ == "__main__":
    main()
