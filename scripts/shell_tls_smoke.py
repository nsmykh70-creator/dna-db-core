import json
from pathlib import Path
import os
import socket
import subprocess
import tempfile
import time

PROJECT = Path(__file__).resolve().parent.parent
BIN = PROJECT / ("target/release/dna-db-core.exe" if os.name == "nt" else "target/release/dna-db-core")


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def wait_tls(port):
    import ssl
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    for _ in range(300):
        try:
            raw = socket.create_connection(("127.0.0.1", port), timeout=2)
            ctx.wrap_socket(raw, server_hostname="localhost").close()
            return
        except Exception:
            time.sleep(0.05)
    raise RuntimeError("TLS listener did not start")


data_dir = tempfile.mkdtemp(prefix="dna_shell_tls_")
port = free_port()
log = open(os.path.join(tempfile.gettempdir(), "dna_shell_tls.log"), "wb")
proc = subprocess.Popen(
    [BIN, "serve", "--data-dir", data_dir, "--port", str(port),
     "--bind", "127.0.0.1", "--admin-port", "0", "--no-scrub", "--sync", "strict"],
    stdout=log, stderr=log, cwd=PROJECT,
)
try:
    wait_tls(port)
    ca = os.path.join(data_dir, "tls", "cert.pem")
    out = subprocess.run(
        [BIN, "shell", "--host", "127.0.0.1", "--port", str(port), "--tls-ca", ca],
        input=".status\n.quit\n", capture_output=True, text=True, timeout=30, cwd=PROJECT,
    )
    result = {
        "returncode": out.returncode,
        "stdout": out.stdout[-2000:],
        "stderr": out.stderr[-2000:],
        "tls_marker": "[TLS]" in out.stdout,
        "connected": "Connected to" in out.stdout,
        "status_ok": "status" in out.stdout.lower(),
    }
    print(json.dumps(result, indent=2, ensure_ascii=False))
    assert out.returncode == 0, out.stderr
    assert result["tls_marker"] and result["connected"], out.stdout
finally:
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
    log.close()
