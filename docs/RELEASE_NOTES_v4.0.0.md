# DNA-DB Enterprise v4.0.0-GA — Release Notes

**Дата:** Октябрь 2026 · **Базовый код:** v3.5.0 (d1129bae) → полный аудит и доработка

## Что сделано в этом релизе (реально, с тестами)

### v3.6 «Unified Query Engine» — SQL диалект доведён до 32/32
- `RIGHT [OUTER] JOIN` и `FULL [OUTER] JOIN` (hash-join, FULL — обе стороны).
- `UNION [ALL]` (дедупликация, имена колонок — от первого SELECT, ORDER BY/LIMIT на соединение).
- Скалярные подзапросы в SELECT и WHERE, **коррелированные** (переписывание внешних ссылок на значения строки), `EXISTS` / `NOT EXISTS`, коррелированный `IN (SELECT ...)`.
- CTE: `WITH name AS (SELECT ...), ...` (statement-scoped materialisation, join'ы по CTE).
- `INSERT ... ON CONFLICT (pk) DO NOTHING | DO UPDATE SET ... = EXCLUDED.col` (UPSERT).
- Оконные функции: `ROW_NUMBER()`, `RANK()`, `DENSE_RANK() OVER (PARTITION BY ... ORDER BY ...)`.
- Сессии: `BEGIN` / `COMMIT` / `ROLLBACK` — DML буферизуется и коммитится ОДНИМ атомарным WAL-кадром; ROLLBACK отбрасывает.
- `SHOW TABLES`, `SELECT VERSION()`, `EXPLAIN <stmt>` (фикс двойного потребления токена), FROM-less `SELECT (SELECT ...)`.
- Неизвестная таблица в FROM/JOIN теперь честная ошибка 42P01-стиля.
- Новый клиентский тест-файл `tests/sql_v36_features.rs` (13 сценариев).

### v3.7 «Ecosystem & pgwire Bridge»
- Новый модуль `src/pgwire.rs`: PostgreSQL Wire Protocol v3 (порт 5432): SSLRequest→'N', startup, AuthenticationOk, ParameterStatus, BackendKeyData, Simple Query (RowDescription/DataRow/CommandComplete/ErrorResponse/ReadyForQuery), Terminate, отмена; текстовые типы int8/float8/text, NULL → NULL.
- `serve --pgwire-port 5432` (0 = выкл). Один и тот же SqlEngine и ACL на обоих протоколах.
- E2E-клиент `tests/test_pgwire_client.py` (сырой протокол): 8 шагов PASS — handshake, VERSION(), DDL/DML, SELECT, оконные функции через pgwire, NULL, ErrorResponse, BEGIN/ROLLBACK/COMMIT.

### v3.8 «Distributed Raft Consensus»
- Новый модуль `src/raft.rs` + подкоманда `cluster`: кворумный Raft (3+ нод) — выборы со случайным джиттером (node-id-подмешанным — фикс расщепления голосов), персистентный term/votedFor/log (fsync), репликация с nextIndex/matchIndex и откатом по conflict_index, commit по большинству, применение к движку одним WAL-батчем, клиентские редиректы NOT_LEADER.
- Интеграционный тест `tests/raft_cluster.rs`: 3 ноды in-process — выборы, репликация 3/3, **failover за 1.05 c (< 2 c SLA)**, ноль потерь закоммиченных записей, сходимость выживших.

### v3.9 «Cloud Tiering & Online DDL»
- `src/tiered_storage.rs`: S3-совместимый клиент (AWS SigV4, HTTP + HTTPS/rustls, HMAC-SHA256 без новых зависимостей), демоушн/промоушн сегментов в S3/MinIO/GCS-S3 (Bloom-сайдкар), удаление локальной копии только после успешной загрузки.
- Online DDL: `ALTER TABLE t ADD COLUMN c T` — мгновенно, только метаданные, без блокировок и перезаписи строк.
- PITR: подкоманда `pitr --target` (консистентный физический снапшот с манифестом) и `pitr --restore`.
- Тесты `tests/tiering_v39.rs`: S3 round-trip через in-process S3-совместимый сервер (проверка подписи), мгновенный ALTER.

### v4.0 «Enterprise Production GA»
- `/metrics` — Prometheus 0.0.4 (реальные счётчики: health, lesions found/repaired/unrecoverable, memtable, segments, stats).
- Kubernetes-манифесты обновлены: CRD `DnaDbCluster`, StatefulSet, Service+ServiceMonitor для Prometheus.

## Результаты финальных прогонов (эта сборка)
- `cargo test --release`: **215 тестов PASS** (184 unit + 32 SQL-фичи + 13 recovery + Raft + tiering + pgwire unit).
- SQL conformance (канонический `scripts/sql_feature_test.py`): **32/32 ok, 0 unsupported, 0 mismatch, 0 error**.
- Chaos suite (`scripts/chaos_test.py`): **3/3 PASS**, 300 000 верификаций ключей, seed=20261021, 0 потерь/искажений.
- Бенчмарк v4.0.0 (`docs/bench_compare_results_v400_linux.json`): batch-load **308 423 rows/s**, point-read p50 0.02 ms, SQL SELECT pk **27 074 ops/s**, загрузка SQL **33 023 ops/s**.
- Сравнение с MySQL 8.4.6 / PostgreSQL 17.11 — эталонные числа прежней платформы сохранены в `docs/bench_compare_results_v350_linux.json` (та же методология, `scripts/bench_compare.py`).

## Честные ограничения
- Raft: одна группа (без шардирования), без снапшотов/компакции лога; линейные записи, чтения на лидере.
- pgwire: simple query protocol; подготовленные выражения — явная понятная ошибка 0A000.
- S3-клиент: SigV4 без multipart (сегменты ≤ 5 ГБ укладываются).
- ALTER TABLE: только ADD COLUMN (metadata-only).
