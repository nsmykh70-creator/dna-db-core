# DNA-DB Core v3.3.0 — mTLS, key rotation, clean audit, benchmark evidence

**Дата:** 2026-10-26  
**Тип:** production security + verification release  
**Совместимость:** SQL, DnaQL, формат WAL/SST, parity/self-healing sidecars, чтение старых plaintext-БД — без изменений.

## Что вошло

### 1. Mutual TLS

- `serve --tls-client-ca CA.pem` требует клиентский сертификат, подписанный этим CA, **и на TCP-порту, и на HTTPS-панели**.
- `shell --tls-client-cert client.pem --tls-client-key client.key` — mTLS-клиент для CLI.
- Без клиентского сертификата TLS 1.3-соединение закрывается до аутентификации; проверено smoke-тестом.
- mTLS — транспортная аутентификация. Она не заменяет пользовательскую аутентификацию и не даёт per-table ACL.

### 2. Offline-ротация ключа

```bash
# сервер остановлен; новый ключ — вне DATA
dna-db-core rotate-key \
  --data-dir ./dna_data \
  --new-key-file /secure/dna-new.key
```

- Читает все живые логические строки старым ключом.
- Пишет их в новый каталог с новым ключом через `write_batch` (strict fsync).
- Сохраняет `users.json`, `security_state.json` и `tls/`.
- Атомарно меняет каталоги; старая копия остаётся в `DATA.pre-rotate-<timestamp>`.
- Поддерживает миграцию старой plaintext-БД в зашифрованную.
- Если в БД есть `DNAE1`-конверты, а старый ключ не передан, команда отказывается работать вместо порчи данных.
- Внешнее зеркало автоматически не мигрируется: после ротации его нужно пересоздать/пересеять.
- Ограничение: операция offline и требует свободного места под вторую копию и RAM под merged scan.

### 3. `cargo audit`: 0 vulnerabilities, 0 warnings

`lru 0.7.8` (RUSTSEC-2026-0253) и `instant 0.1.13` (RUSTSEC-2024-0384) приходили через `reed-solomon-erasure 6.0.0`. Добавлена vendored byte-compatible копия `vendor/reed-solomon-erasure`:

- тот же GF(2^8)-кодек и формат parity;
- `lru` обновлён до 0.19, `parking_lot` — до 0.12;
- старые WAL/SST parity и self-healing sidecars остаются читаемыми;
- upstream MIT license сохранена.

### 4. Benchmark

Новый отчёт: `BENCHMARK_REPORT_2026-10-26_RU.md` (в репозитории — `docs/BENCHMARK_2026-10-26_RU.md`).

Кратко, 1 млн строк по 160 байт, 32 клиента, 0 ошибок:

| Система | SELECT, ops/s | UPDATE, ops/s | Занято, MiB |
|---|---:|---:|---:|
| DNA-DB TLS+AEAD | **6 876** | 1 060 | 750,2 |
| PostgreSQL 17 | 5 750 | **3 341** | 212,9 |
| Oracle 23ai Free | 4 306 | 2 500 | 208,0 |
| MySQL 8.4 | 3 183 | 1 134 | 174,7 |

Важно: 32 клиента — верхняя точка прогона, а не найденная точка падения. MySQL/Oracle вышли на плато по SELECT на 16 клиентах; DNA-DB/PostgreSQL на 32 ещё росли. Цена self-healing — примерно 3,5× места относительно MySQL.

## Проверка

| Проверка | Результат |
|---|---|
| `cargo test --release` | **176 passed** (161 unit + 2 encryption + 13 recovery) |
| `cargo audit` | **0 vulnerabilities, 0 warnings** |
| `security_probe.py` | **12/12 PASS** |
| TLS panel + shell smoke | PASS |
| mTLS smoke | **2/2 PASS** |
| key rotation smoke | **2/2 PASS** |
| fixed benchmark | DNA-DB TLS+AEAD: 1 004 load/s, 32 942 batch rows/s, 3 771 read/s, 3 491 SQL SELECT/s |
| 1M-row benchmark | DNA-DB 3 019 rows/s load, 6 876 SELECT/s, 1 060 UPDATE/s |

## Остаточные ограничения

Они не выдаются за исправленные:

- **LSM-ключи и метаданные не шифруются.** Это необходимо для ordered scan/prefix lookup; имена ключей нужно защищать на уровне приложения.
- **Нет per-table ACL.** Роли `admin`/`operator`/`viewer` глобальные; для table-level изоляции — отдельный инстанс или сетевой слой.
- **Нет внешнего IdP (OIDC/SAML).** mTLS не заменяет identity provider.
- **Ротация ключа offline.** Online re-encryption без остановки сервиса не реализован.
- **Нет HSM/KMS.** Ключ можно передать из secret manager через окружение, но сам движок HSM не поддерживает.

Эти пункты требуют отдельных архитектурных изменений; они явно перечислены в `SECURITY.md` / `SECURITY_RU.md`, а не скрыты в маркетинговом тексте.

## Обновление

1. Остановите сервер.
2. Сделайте backup `DATA` и ключа.
3. Замените бинарник и запустите `serve` как обычно. Формат данных совместим; автоматическая миграция не требуется.
4. Для ротации ключа используйте `rotate-key`, проверьте данные, затем удалите `DATA.pre-rotate-*` и старый ключ.
5. Для mTLS добавьте `--tls-client-ca` и настройте клиентские сертификаты.


## ?????????

- `dna-db-core-v3.3.0-windows-x86_64.zip`
- SHA-256: `b3bb8c5d0a4d0fd699a8085b658e1fc3476f0fb838c464ea945bcdd2072e5024`
- `BENCHMARK_REPORT_2026-10-26_RU.md`
- `security_results_2026-10-26_public_v3.3.0.json`
