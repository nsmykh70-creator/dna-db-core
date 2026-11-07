# DNA-DB CMS Edition — E2E: спирали dnadb + веб-панель баз данных

Дата: 7 ноября 2026. Ветка: `cms-edition`. Драйвер:
`scripts/e2e_spirals_panel.sh` — реальный `dna-db-core mysqld` с панелью
(`--admin-port 13310`), curl против JSON-API, клиент `mariadb` против wire
(:13307), ethалонные проверки манифестов Python'ом.

Приёмочные пункты Прогона №11 (запрос продукта):

1. **При создании новой БД появляются две новые спирали dnadb** (Strand A +
   Strand B);
2. **базы создаются, удаляются и правятся из веб-панели**;
3. **в панели выбираются хранилища спиралей: локальные диски или сетевые
   хранилища**;
4. **устранена ошибка из отчётов** — дисплейная шкала SUM-агрегатов
   (Прогон №10: `250.1` вместо `250.10`).

## Что проверяется

- **Панель.** `GET /` отдаёт embedded SPA (русский UI); `POST /api/login`
  по креденциалам `--user root:dnatest` → bearer-токен (12 ч); неверный
  пароль → `bad credentials`; запрос без токена → 401.
- **Создание БД из панели.** `POST /api/databases {name: shop,
  strand_a: {label: local-a}, strand_b: {path: <net>, kind: network}}` →
  на диске появляются **две реальные спирали**: `<data>/spirals-a/shop.spiral`
  (локальный корень) и `<net>/shop.spiral` (сетевое хранилище — любой
  абсолютный путь), обе — движком инициализированные каталоги данных
  (WAL/сегменты, не пустые папки), обе с `spiral_manifest.json`
  (pair_id, роль A/B, kind, root_label, версия движка); манифесты
  консистентны (один pair_id, роли разные, B — network).
- **Создание БД через SQL.** `CREATE DATABASE webdb` через wire-клиент —
  та же пара спиралей на корнях по умолчанию (`spirals-a`/`spirals-b`);
  `SHOW DATABASES` видит и панельную, и SQL-базу (единый каталог).
- **Хранилища.** `GET /api/storage` — зарегистрированные корни +
  обнаруженные маунты (`/proc/mounts`, write-проба; в песочнице кандидатов
  0 — корень `/` не доступен на запись, на реальном хосте список заполняет-
  ся локальными дисками и nfs/cifs); `POST /api/storage` — регистрация
  сетевого корня `nas` (с write-пробой).
- **Правка (перенос) спирали.** `POST /api/databases/shop/relocate
  {role: B, label: nas}` — copy → verify манифеста → swap → обновление
  каталога → удаление старой копии; манифест на новом месте подтверждает
  root_label=nas; `DELETE /api/databases/shop` снимает обе спирали с диска.
- **SQL DROP.** `DROP DATABASE webdb` удаляет обе спирали и каталог.
- **Фикс SUM.** `SELECT SUM(price)` над decimal(10,2) печатает **`250.10`**
  (было `250.1`) — колонка агрегата несёт MYSQL_TYPE_NEWDECIMAL с
  decimals=2 в обоих протоколах.

## Результат

```
== mysqld up (wire :13307, panel :13310)
[MYSQLD] spiral catalog: 0 database pair(s) on storage roots local-a/local-b
== panel: login ok, auth enforced
pair_id pair-05292682bedd87b2 A/B manifests consistent
== panel: shop created — two spirals (A on local-a, B on network path)
== sql: webdb created via wire — spirals at …/spirals-a/webdb.spiral + …/spirals-b/webdb.spiral
SHOW DATABASES = information_schema shop webdb wordpress
== storage: candidates detected: 0
strand B relocated to nas, manifest updated
== storage: relocate ok, panel drop removed both spirals
== sql: webdb dropped — spirals removed
== sum fix: SELECT SUM(price) → 250.10 (was '250.1' before the fix)
SPIRALS_PANEL_E2E_VERDICT: OK
```

Регрессии: `e2e_mysqldump_roundtrip.sh` и `e2e_backup_restore.sh` зелёные —
CREATE/DROP DATABASE внутри них теперь физически строит/снимает пары
спиралей, ничто не сломалось.

## Реализация

- `src/cms/spirals.rs` — каталог пар (`spirals.json`, атомарная запись),
  корни хранилищ (write-проба), детект маунтов, создание пары (два
  движком-инициированных каталога + манифесты), удаление, перенос;
  имя БД `[A-Za-z0-9_$]` — path traversal невозможен; 5 юнит-тестов.
- `src/cms/panel.rs` + `src/cms/panel.html` — HTTP-сервер панели
  (без новых зависимостей), bearer-токены, lockout 5/мин/IP; API:
  `/api/login|status|databases|storage` (list/create/delete/relocate/add/
  remove); русский SPA-UI: список баз с обеими спиралями, форма создания
  (селектор хранилища или путь вручную на каждую спираль), менеджер
  хранилищ с кнопкой «Добавить» у обнаруженных дисков; 2 юнит-теста
  (percent-decode, HTTP-парсер).
- Гейт `CmsSqlGate`: `attach_spiral_catalog(data_dir)` при старте mysqld;
  CREATE/DROP DATABASE физически работают с парами; `panel_*`-методы —
  общий каталог для SQL и панели; SHOW DATABASES объединяет оба источника.
- `mysqld --admin-port N --admin-bind …` — запуск панели; порт wire не
  трогается.
- Фикс SUM: `cms::compat::decimal::resolve_scale` + `aggregate_inner_scale`
  — SUM/MIN/MAX сохраняют scale аргумента, AVG расширяет на 4 (правило
  MySQL); тест `decimal_aggregate_scale_parity` (gate.rs).

## Ограничение (честно)

Табличные данные активного каталога живут в основном data-dir движка;
пары спиралей — per-БД слой хранения и метаданные размещения, к которому
подключается per-БД движок в Фазе 4 (контракт зафиксирован манифестами и
каталогом). Это отражено в док-комментарии `spirals.rs` и в
CMS_EDITION_RU.md.

## Тесты

341 с `--features cms` (+9), 195 без (+4 известных отказа cms-батарей —
база не тронута). Релизный бинарь обновлён в `prebuilt/linux-x86_64/`
(SHA256SUMS пересчитан).
