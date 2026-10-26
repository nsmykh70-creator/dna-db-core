#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Mutual-TLS smoke test.

Generates a throwaway CA, server certificate and client certificate, starts
DNA-DB with --tls-client-ca, then verifies that:
  * a client with a CA-signed certificate can connect and authenticate;
  * a client without a certificate is rejected by the TLS handshake.
"""

import datetime
import os
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.x509.oid import ExtendedKeyUsageOID, NameOID

from bench_compare import DnaDbClient, free_port, find_binary

PROJECT = Path(__file__).resolve().parent.parent


def key():
    return rsa.generate_private_key(public_exponent=65537, key_size=2048)


def name(cn):
    return x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, cn)])


def write_key(path, k):
    path.write_bytes(k.private_bytes(
        serialization.Encoding.PEM,
        serialization.PrivateFormat.PKCS8,
        serialization.NoEncryption(),
    ))


def write_cert(path, cert):
    path.write_bytes(cert.public_bytes(serialization.Encoding.PEM))


def make_pki(dir):
    now = datetime.datetime.now(datetime.timezone.utc)
    ca_key = key()
    ca = (
        x509.CertificateBuilder()
        .subject_name(name("DNA-DB test CA"))
        .issuer_name(name("DNA-DB test CA"))
        .public_key(ca_key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(minutes=5))
        .not_valid_after(now + datetime.timedelta(days=1))
        .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
        .sign(ca_key, hashes.SHA256())
    )

    def issue(cn, san, eku):
        k = key()
        b = (
            x509.CertificateBuilder()
            .subject_name(name(cn))
            .issuer_name(ca.subject)
            .public_key(k.public_key())
            .serial_number(x509.random_serial_number())
            .not_valid_before(now - datetime.timedelta(minutes=5))
            .not_valid_after(now + datetime.timedelta(days=1))
            .add_extension(x509.SubjectAlternativeName(san), critical=False)
            .add_extension(x509.ExtendedKeyUsage([eku]), critical=False)
            .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
        )
        return k, b.sign(ca_key, hashes.SHA256())

    server_key, server_cert = issue(
        "localhost",
        [x509.DNSName("localhost"), x509.IPAddress(__import__("ipaddress").ip_address("127.0.0.1"))],
        ExtendedKeyUsageOID.SERVER_AUTH,
    )
    client_key, client_cert = issue(
        "dna-db-client",
        [x509.DNSName("dna-db-client")],
        ExtendedKeyUsageOID.CLIENT_AUTH,
    )
    write_cert(dir / "ca.pem", ca)
    write_key(dir / "server.key", server_key)
    write_cert(dir / "server.pem", server_cert)
    write_key(dir / "client.key", client_key)
    write_cert(dir / "client.pem", client_cert)
    return dir / "ca.pem", dir / "server.pem", dir / "server.key", dir / "client.pem", dir / "client.key"


def wait_ready(port, ca, client_cert, client_key, proc, log_path, timeout=20):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError("server died; log: %s" % log_path.read_text(errors="replace"))
        try:
            c = DnaDbClient("127.0.0.1", port, "admin", "mtls-pass",
                            cafile=str(ca), certfile=str(client_cert),
                            keyfile=str(client_key))
            c.close()
            return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError("server did not become ready")


def main():
    tmp = Path(tempfile.mkdtemp(prefix="dna_mtls_"))
    proc = None
    try:
        pki = tmp / "pki"
        pki.mkdir()
        ca, cert, keyf, client_cert, client_key = make_pki(pki)
        data = tmp / "data"
        port = free_port()
        log_path = tmp / "server.log"
        log = open(log_path, "wb")
        proc = subprocess.Popen(
            [find_binary(), "serve",
             "--data-dir", str(data),
             "--port", str(port),
             "--bind", "127.0.0.1",
             "--admin-port", "0",
             "--admin-pass", "mtls-pass",
             "--tls-cert", str(cert),
             "--tls-key", str(keyf),
             "--tls-client-ca", str(ca),
             "--no-encryption",
             "--no-scrub"],
            stdout=log, stderr=log, cwd=str(PROJECT),
        )
        wait_ready(port, ca, client_cert, client_key, proc, log_path)

        # Positive: certificate + auth.
        c = DnaDbClient("127.0.0.1", port, "admin", "mtls-pass",
                        cafile=str(ca), certfile=str(client_cert),
                        keyfile=str(client_key))
        c.close()
        print("PASS: client certificate accepted and authenticated")

        # Negative: TLS client without a certificate must be rejected.
        ctx = ssl.create_default_context(cafile=str(ca))
        raw = socket.create_connection(("127.0.0.1", port), timeout=5)
        rejected = True
        try:
            tls = ctx.wrap_socket(raw, server_hostname="127.0.0.1")
            # TLS 1.3 asks for the client certificate after the handshake, so
            # the rejection is observed on the first application read.
            tls.sendall(b"\x01admin:mtls-pass")
            tls.settimeout(5)
            ok = tls.recv(2)
            rejected = ok != b"OK"
            tls.close()
        except (ssl.SSLError, ConnectionError, OSError):
            rejected = True
        finally:
            raw.close()
        if not rejected:
            print(log_path.read_text(errors="replace"))
            raise RuntimeError("server accepted a client without a certificate")
        print("PASS: client without certificate rejected")
        print("RESULT: 2/2 PASS")
        return 0
    finally:
        if proc and proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
