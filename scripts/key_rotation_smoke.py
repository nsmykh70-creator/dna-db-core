#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""End-to-end offline key-rotation smoke test.

Starts an encrypted+TLS server, writes a row, stops it, runs `rotate-key`,
starts the server again with the new key and verifies the row still decrypts.
The old key/backup must remain separate from the live directory.
"""

import os
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

from bench_compare import DnaDbClient, free_port, find_binary

PROJECT = Path(__file__).resolve().parent.parent


def wait_ready(port, cafile, proc, log_path, timeout=20):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError("server died:\n" + log_path.read_text(errors="replace"))
        try:
            c = DnaDbClient("127.0.0.1", port, "admin", "rotate-pass",
                            cafile=str(cafile))
            c.close()
            return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError("server did not become ready")


def start(data, port, key_file, log_path):
    log = open(log_path, "ab")
    cmd = [find_binary(), "serve", "--data-dir", str(data),
           "--port", str(port), "--bind", "127.0.0.1",
           "--admin-port", "0", "--admin-pass", "rotate-pass",
           "--encryption-key-file", str(key_file),
           "--no-scrub"]
    return subprocess.Popen(cmd, stdout=log, stderr=log, cwd=str(PROJECT)), log


def stop(proc, log):
    if proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
    log.close()


def main():
    tmp = Path(tempfile.mkdtemp(prefix="dna_rotate_e2e_"))
    data = tmp / "data"
    new_key = tmp / "new.key"
    log_path = tmp / "server.log"
    proc = None
    log = None
    try:
        port = free_port()
        proc, log = start(data, port, data / ".dna-db.key", log_path)
        wait_ready(port, data / "tls" / "cert.pem", proc, log_path)
        c = DnaDbClient("127.0.0.1", port, "admin", "rotate-pass",
                        cafile=str(data / "tls" / "cert.pem"))
        c.put(b"rotate:1", b"value-before-rotation")
        c.close()
        stop(proc, log)
        proc = log = None

        proc = subprocess.run(
            [find_binary(), "rotate-key", "--data-dir", str(data),
             "--new-key-file", str(new_key)],
            cwd=str(PROJECT), capture_output=True, text=True, timeout=300,
        )
        if proc.returncode != 0:
            raise RuntimeError("rotate-key failed:\n%s\n%s" % (proc.stdout, proc.stderr))
        if not new_key.exists():
            raise RuntimeError("new key file was not created")
        backups = list(tmp.glob("data.pre-rotate-*"))
        if not backups:
            raise RuntimeError("rollback backup was not created")
        print("PASS: rotate-key completed; backup %s" % backups[0].name)

        port = free_port()
        proc, log = start(data, port, new_key, log_path)
        wait_ready(port, data / "tls" / "cert.pem", proc, log_path)
        c = DnaDbClient("127.0.0.1", port, "admin", "rotate-pass",
                        cafile=str(data / "tls" / "cert.pem"))
        got = c.get(b"rotate:1")
        c.close()
        if got != b"value-before-rotation":
            raise RuntimeError("row did not decrypt with the new key: %r" % got)
        print("PASS: row decrypts with the new key after restart")
        print("RESULT: 2/2 PASS")
        return 0
    finally:
        if proc and log:
            stop(proc, log)
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
