# DNA-DB Core v3.4.0 — per-table ACL, trusted-proxy SSO, mirror-aware rotation

**Дата:** 2026-10-30  
**Тип:** production security + access-control release  
**Совместимость:** SQL, DnaQL, формат WAL/SST, parity/self-healing sidecars, чтение старых `users.json` и plaintext-БД — без миграции.

## Что вошло

### 1. Per-table ACL

- У пользователя появился необязательный список `allowed_tables` в `users.json`.
  - нет поля или `*` — без ограничений (поведение старых БД);
  - пустой список — запрет всех таблиц;
  - иначе — только перечисленные таблицы, сравнение без учёта регистра.
- ACL проверяется **до выполнения** запроса по разобранному AST:
  - SQL: `SELECT`/`JOIN`, `IN (SELECT ...)`, `INSERT`/`UPDATE`/`DELETE`, `CREATE/DROP TABLE`, `CREATE/DROP INDEX`, `EXPLAIN`, внешние ключи;
  - DnaQL: `FIND`, `FROM ... |> LINK`, транзакционные операции, `VERIFY`/`REPAIR`.
- Панель фильтрует `/api/tables`, проверяет `/api/browse` и `/api/query`.
- Для ограниченных пользователей отключены raw GET/PUT/DELETE/CHECKPOINT: у KV-протокола нет табличного пространства, иначе ACL обходится одним `GET`.
- Schema-wide `SHOW`/`DESCRIBE` ограниченным пользователям запрещён.
- Смена ACL отзывает все сессии этого пользователя.
- В панели: поле «таблицы» при создании и кнопка «Доступ» для редактирования.

Пример:

```bash
# create a viewer limited to two tables through the panel API
curl -k https://127.0.0.1:8080/api/users \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"user":"alice","pass":"alice-pass-123","role":"viewer",
       "must_change":false,"allowed_tables":["chr_users","chr_orders"]}'
```

### 2. Внешний IdP через доверенный reverse proxy

- `serve --trusted-proxy IP` (можно повторять) разрешает `POST /api/login/proxy` только с указанного peer IP.
- Логин передаётся в `X-Auth-Request-User` (`--proxy-auth-header`).
- Опциональный shared secret: `--proxy-auth-secret` / `DNA_DB_PROXY_AUTH_SECRET`, заголовок `X-Proxy-Auth-Secret` (`--proxy-auth-secret-header`), сравнение constant-time.
- SPA автоматически пытается выполнить proxy-вход, если нет локальной сессии.
- Подходит для oauth2-proxy, Authentik, nginx `auth_request` и аналогичных схем.
- **Proxy обязан вырезать клиентские identity/secret-заголовки.** Это интеграция с внешним IdP, а не встроенный OIDC/SAML-клиент.

```bash
dna-db-core serve --data-dir ./dna_data \
  --trusted-proxy 127.0.0.1 \
  --proxy-auth-header X-Auth-Request-User \
  --proxy-auth-secret "$DNA_DB_PROXY_AUTH_SECRET"
```

### 3. Ротация ключа с внешним зеркалом

```bash
# сервер остановлен; новый ключ — вне DATA
dna-db-core rotate-key \
  --data-dir ./dna_data \
  --new-key-file /secure/dna-new.key \
  --mirror-dir /mnt/dna_mirror
```

- После перешифрования primary внешнее зеркало Strand B **пересоздаётся** из нового primary и проверяется открытием с новым ключом.
- Старое зеркало сохраняется как `<mirror>.pre-rotate-<timestamp>`.
- Если пересоздание/подмена зеркала не удалась, primary откатывается на старую копию; оператор не остаётся с новым primary и старым зеркалом.
- Без `--mirror-dir` поведение прежнее: зеркало нужно пересеять вручную.

### 4. Проверки

| Проверка | Результат |
|---|---|
| `cargo test --release` | **186 passed** (171 unit + 2 encryption + 13 recovery) |
| `cargo audit` | **0 vulnerabilities, 0 warnings** (1386 advisories) |
| `scripts/acl_proxy_smoke.py` | **24/24 PASS** |
| `scripts/key_rotation_smoke.py` | **2/2 PASS**, включая пересоздание зеркала |
| `scripts/security_probe.py` | **12/12 PASS** |
| `scripts/tls_smoke.py` | **PASS** |
| `scripts/smoke_test.py` | **15/15 PASS** (сервер поднимается с `--no-tls`) |
| mTLS smoke | 2/2 PASS (не изменялся) |

ACL smoke проверяет: список таблиц в панели, запрет browse/query, запрет JOIN и подзапроса к чужой таблице, блокировку raw KV, DnaQL, proxy-вход с секретом и без, отзыв сессии при смене ACL.

### 5. Benchmark

Новых измерений в этом релизе нет: v3.4 меняет control plane (аутентификация/ACL/proxy/ротация), а не storage-write path. Актуальные измеренные данные остаются в `docs/BENCHMARK_2026-10-26_RU.md`:

- fixed production DNA-DB: 1 004 load/s, 32 942 batch rows/s, 3 771 point read/s, 3 786 mixed/s, 3 491 SQL SELECT/s;
- max 1M: 3 019 rows/s load, 6 876 SELECT/s @32, 1 060 UPDATE/s @32, 750,2 MiB;
- PostgreSQL 17: 6 293 load/s, 5 750 SELECT/s, 3 341 UPDATE/s, 212,9 MiB;
- Oracle 23ai Free: 1 494 load/s, 4 306 SELECT/s, 2 500 UPDATE/s, 208,0 MiB;
- MySQL 8.4: 3 003 load/s, 3 183 SELECT/s, 1 134 UPDATE/s, 174,7 MiB.

Условия сравнения прежние: DNA-DB работает нативно в Windows, MySQL/PostgreSQL/Oracle — в Docker/WSL2; 32 клиента — верхняя точка теста, а не точка падения.

## Остаточные ограничения

Они **не** выдаются за исправленные:

- **LSM-ключи и метаданные не шифруются.** Record keys, длины, число строк и паттерны доступа видны при raw-disk доступе. Нужен формат v2 или компромисс с утечкой порядка.
- **Встроенного OIDC/SAML нет.** Есть только trusted-proxy интеграция; её безопасность зависит от корректной конфигурации proxy.
- **ACL — allowlist, не row-level security.** Пользователь видит все строки разрешённой таблицы; row-фильтров и маскирования колонок нет.
- **Нет HSM/KMS.** Ключ можно передать из secret manager через окружение, но сам движок PKCS#11/cloud KMS не поддерживает.
- **Ротация offline.** Online re-encryption без остановки сервиса не реализован.
- **Audit-log export не реализован.**

## Обновление

1. Остановите сервер.
2. Сделайте backup `DATA`, ключа и внешнего зеркала.
3. Замените бинарник и запустите `serve` как обычно. Старые `users.json` читаются: отсутствие `allowed_tables` означает отсутствие ограничений.
4. Создайте ACL через панель или API и проверьте доступ ограниченным пользователем.
5. Для SSO настройте reverse proxy, который **перезаписывает** `X-Auth-Request-User` и `X-Proxy-Auth-Secret`, и запустите сервер с `--trusted-proxy`.
6. Для ротации с зеркалом используйте `rotate-key --mirror-dir`; после проверки удалите `DATA.pre-rotate-*`, `<mirror>.pre-rotate-*` и старый ключ.

## Артефакты

- `dna-db-core-v3.4.0-windows-x86_64.zip`
- SHA-256: `002f6bce1403e041a0759e1abd7b99076b40aa947e2d8f357b8368c08da9276e`
- `dna-db-core-v3.4.0-windows-x86_64.zip.sha256` — тот же SHA для `sha256sum -c`
- `BENCHMARK_REPORT_2026-10-26_RU.md`
- `security_results_2026-10-30_public_v3.4.0.json`

Опубликовано: https://github.com/nsmykh70-creator/dna-db-core/releases/tag/v3.4.0
В самом архиве лежит эта же копия без строки SHA-256: файл не может
содержать контрольную сумму архива, в который он упакован. Проверяйте
скачанный архив по `.sha256` рядом с ним.
