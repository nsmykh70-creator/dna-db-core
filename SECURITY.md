# DNA-DB Core security model

This document describes the production security controls in DNA-DB Core
v3.3, the operational assumptions behind them, and the residual risks we have
not eliminated. It is intentionally narrower than a compliance certification:
DNA-DB is a single-node database, not a managed service.

## Supported version

| Version | Security support |
|---|---|
| `3.3.x` | current production hardening release (mTLS, key rotation, clean `cargo audit`) |
| `3.2.x` | previous hardening release; upgrade for mTLS/key rotation |
| `<= 3.1` | no TLS, SHA-256 password hashing, no at-rest payload encryption — do not expose |

Report vulnerabilities privately via the repository security advisory UI or
the maintainer contact in `Cargo.toml`; please do not publish a working exploit
before a fix is available.

## Controls

### Transport

* Both the binary TCP protocol and the web panel use **rustls** with TLS 1.2
  and TLS 1.3 (`ring` provider). There is no plaintext fallback on a TLS
  listener.
* `serve` enables TLS by default. A self-signed localhost certificate
  (`DATA/tls/cert.pem` + `key.pem`) is generated on first start. For a public
  DNS name supply `--tls-cert` / `--tls-key` (for example Let's Encrypt).
* **Mutual TLS is available**: pass `--tls-client-ca CA.pem` to require a
  client certificate signed by that CA on both the TCP and HTTPS listeners.
  `shell` accepts `--tls-client-cert` and `--tls-client-key`. Without a client
  certificate the TLS 1.3 handshake is terminated before authentication.
* HTTPS responses include
  `Strict-Transport-Security: max-age=31536000; includeSubDomains`.
* `--no-tls` exists for local diagnostics and legacy benchmark clients. Never
  use it on an untrusted network.

### Authentication and users

* Passwords are stored as **Argon2id PHC strings** in `DATA/users.json`, with a
  fresh random salt per password. Plaintext passwords are never written.
* Unknown usernames still execute a dummy Argon2 verification so response
  timing does not disclose account existence.
* A new data directory is bootstrapped with **`admin` / `password`**. This is
  deliberately convenient for evaluation and must be changed immediately in
  the panel. `--admin-pass` or `DNA_DB_ADMIN_PASS` overrides the first-start
  password.
* The panel supports create, disable, delete, reset-password and self
  password-change. The last active `admin` cannot be disabled or deleted.
* Roles:
  * `admin` — user administration plus all data operations;
  * `operator` — data read/write, no user administration;
  * `viewer` — read-only queries and table browsing.
* Roles are enforced on both the HTTPS API and the binary TCP protocol.
  Viewer SQL/DnaQL is accepted only for a conservative read-only allowlist
  (`SELECT`, `SHOW`, `DESCRIBE`, `EXPLAIN`, `PRAGMA`, `ANALYZE`, `FIND`,
  `SEARCH`, `VERIFY`); multi-statement input is rejected.
* Sessions are 256-bit CSPRNG bearer tokens with a 12-hour TTL. They are
  stored only in process memory and are revoked when the user's password is
  changed, reset, or the account is disabled.
* The panel is a bearer-token SPA. Tokens live in `sessionStorage`, not in
  cookies, so classic cookie CSRF does not apply. The API requires an
  `Authorization: Bearer` header.

### Brute force and pre-auth DoS

* Persistent lockout is keyed by **source IP** and **normalised username**
  (5 failures / 60 seconds by default). State is written atomically to
  `DATA/security_state.json` and reloaded on restart.
* A global 2,000-failures-per-minute throttle protects against distributed
  password spraying. It is intentionally coarse and may briefly delay a
  legitimate login during an attack.
* Argon2 verification is serialised behind a process-wide gate. Each hash
  needs roughly 19 MiB; without the gate 128 pre-auth connections could
  reserve gigabytes.
* Pre-authentication frames are capped at **64 KiB**. The 16 MiB cap applies
  only after successful authentication, eliminating the previous
  `128 × 16 MiB` allocation amplification.
* Pre-auth read timeout is 10 seconds; authenticated connections use 300
  seconds; writes use 30 seconds. The panel accepts at most 64 concurrent
  connections and the TCP server 128.

### Web panel

* CSP uses a SHA-256 hash for the single inline SPA `<script>`; scripts do not
  use `'unsafe-inline'`. `style-src` still allows inline styles because the
  SPA uses `style="..."` attributes heavily.
* `X-Frame-Options: DENY`, `frame-ancestors 'none'`, `X-Content-Type-Options:
  nosniff`, `Referrer-Policy: no-referrer`, a restrictive
  `Permissions-Policy`, and `Cache-Control: no-store` are sent on every
  response.
* Request bodies and headers are bounded; the HTTP parser is a small
  purpose-built HTTP/1.1 reader, not a general-purpose proxy.

### Encryption at rest

* Every logical **value** is encrypted with XChaCha20-Poly1305 before it is
  appended to the WAL, written to the Strand B mirror, or packed into an SST
  segment. The record key is authenticated as AEAD associated data, so
  swapping ciphertexts between rows fails authentication.
* Ciphertext envelopes are self-describing (`DNAE1` + nonce + ciphertext).
  Legacy plaintext values remain readable when encryption is first enabled;
  new writes are always encrypted.
* The master key is read from, in order:
  1. `DNA_DB_ENCRYPTION_KEY` — 64 hex characters;
  2. `--encryption-key-file PATH` — 32 raw bytes or 64 hex characters;
  3. `DATA/.dna-db.key` — generated with OS CSPRNG on first start.
* Back up the key **separately** from the data directory. Losing it makes
  values unrecoverable; copying it next to the data defeats the control.
* **Offline key rotation** is supported by `rotate-key`. It opens the source
  directory with the old key, copies every live logical row into a fresh
  directory encrypted with the new key, preserves `users.json`,
  `security_state.json` and `tls/`, then swaps directories. The old tree is
  kept as `DATA.pre-rotate-<timestamp>` for rollback. The command is offline:
  stop the server first, keep the new key outside `DATA`, and delete the
  rollback tree plus old key after verification. External mirror disks are not
  migrated automatically; re-seed the mirror after rotation.
* WAL parity sidecars, SST parity shards, and the mirror contain the same
  ciphertext. Bloom sidecars contain keyed membership bits, not values.

## Deliberate limitations

* **The LSM key space is not encrypted.** Record keys (for example
  `patient:42`), key lengths, segment sizes, row counts and access patterns are
  visible to someone with raw disk access. This is required for ordered scans
  and prefix lookups. Encrypt keys at the application layer if their names are
  sensitive.
* Compression happens above the encrypted payload; on-disk sizes reveal
  approximate value sizes.
* Key rotation is an **offline** copy-and-swap operation, not an online
  re-encryption service. It needs free space for a second copy and RAM for one
  merged scan of the live key space.
* There is no per-table ACL, audit-log export, or external identity provider
  (OIDC/SAML) integration. Roles are global (`admin`, `operator`, `viewer`);
  use a separate database instance or the network layer when table-level
  isolation is required. mTLS is transport authentication, not application
  user identity.
* A self-signed certificate is not identity verification. Use a real CA and
  pass `--tls-ca` to `shell` (or use a normal HTTPS client) for production.
* `--no-encryption` and `--no-tls` are explicit, documented escapes, not
  production defaults.
* The generated `.dna-db.key` is protected by filesystem permissions
  (`0600` on Unix) but not by an HSM/KMS. In high-assurance deployments inject
  the key from a secret manager via the environment and keep it off disk.
* Single-node only: no replication, no distributed consensus, no protection
  against a malicious root on the host.

## Dependency advisories

`cargo audit` on the v3.3 lockfile reports **zero vulnerabilities and zero
warnings**.

The former `lru 0.7.8` and `instant 0.1.13` warnings came transitively through
`reed-solomon-erasure 6.0.0`. The crate is now built from a vendored,
byte-compatible copy (`vendor/reed-solomon-erasure`) that keeps the same
GF(2^8) codec and on-disk parity format but upgrades `lru` to 0.19 and
`parking_lot` to 0.12. Existing WAL/SST parity and self-healing sidecars remain
readable; the change is covered by the full test suite and the rotation smoke
test. The vendored crate retains its upstream MIT license.

## Operational checklist

1. Change `admin/password` before exposing the panel.
2. Put the data directory and the key on different volumes / access domains.
3. Supply a CA-issued certificate; do not rely on the self-signed bootstrap
   certificate outside localhost.
4. Keep `--bind` loopback unless a firewall and TLS are in place.
5. Back up `DATA`, the key, and `users.json`; test restore.
6. Monitor `security_state.json` growth and repeated lockout messages.
7. For high-assurance deployments, enable mTLS (`--tls-client-ca`) and keep
   the client CA private key offline.
8. Rotate payload keys with the server stopped:
   `dna-db-core rotate-key --data-dir DATA --new-key-file /secure/new.key`.
   Verify the server, then delete the `DATA.pre-rotate-*` tree and old key.
9. Run `cargo audit` and `cargo test` on every upgrade.
10. Remember: encryption protects values, not key names or traffic metadata.
