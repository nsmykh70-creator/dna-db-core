# DNA-DB CMS Edition — E2E: MediaWiki 1.43.11 через универсальный MySQL wire-адаптер

Дата: 5 ноября 2026. Ветка: `cms-edition`. Версия движка: v4.0.3 (prebuilt,
feature `cms`, пересборка с фиксом compound-ON outer joins).

## Что проверяется

MediaWiki 1.43.11 (актуальная стабильная 1.43 LTS) устанавливается на DNA-DB
**как на обычный MySQL**: штатный mysqli из статического PHP 8.3.28 →
`dna-db-core mysqld :13306`. Установка — официальный CLI-установщик
(`maintenance/install.php`): он создаёт базу `wikidna`, строит все таблицы
схемы 1.43 (MCR: `slots`/`content`/`text` вместо `rev_text_id`), заливает
дефолтный контент (Main_Page) и создаёт администратора. Префикс `mwd_`.
Дальше — рендер вики через PHP built-in server, правка страницы штатным
`maintenance/edit.php` (запись через wire), API и проверки уровня БД.

## Результат: PASS

| Проверка | Результат |
| --- | --- |
| CLI-установка (`maintenance/install.php`, rc=0) + `LocalSettings.php` | OK |
| Рендер `/index.php/Main_Page` | HTTP 200, sitename «DNA-DB Wiki» |
| Правка `Main_Page` штатным `maintenance/edit.php` | OK |
| Правка видна через `?action=raw` | OK |
| `api.php?action=query&meta=siteinfo` (general+statistics) | OK, `edits: 2` |
| Таблиц `mwd_*` в `wikidna` | 58 |
| Цепочка page → revision → slots → content → text (JOIN-ы, `ORDER BY r.rev_id DESC LIMIT 1`) | согласованная, `page_latest = 2 = last_rev` |
| `recentchanges` | 2 записи (дефолт + правка) |

## Воспроизведение

```bash
dna-db-core mysqld --data-dir /tmp/mw_dna_data --bind 127.0.0.1 --port 13306 \
  --user root:dnatest &
cd mediawiki-1.43.11
php maintenance/install.php \
  --dbtype mysql --dbserver '127.0.0.1:13306' \
  --dbname wikidna --dbuser root --dbpass dnatest \
  --installdbuser root --installdbpass dnatest \
  --dbprefix mwd_ --scriptpath '' --pass 'DnaDb!MW2026x' \
  'DNA-DB Wiki' dnadmin
# ожидание: rc=0, LocalSettings.php записан
php -S 127.0.0.1:8080 -t . &
curl -s http://127.0.0.1:8080/index.php/Main_Page          # HTTP 200
echo 'text' | php maintenance/run.php edit --user dnadmin --summary 'E2E' Main_Page
curl -s 'http://127.0.0.1:8080/api.php?action=query&meta=siteinfo&siprop=general%7Cstatistics&format=json'
```

Драйвер прогона: `scripts/e2e_mediawiki_mysqlwire.sh` (старт сервера,
ожидание порта, установка, рендер, правка, API, проверки БД — одним вызовом).

## Квирки MediaWiki, закрытые движком ранее (раунды 7c01735 / d8cac12)

`information_schema`-шимы (schemata/tables/columns/statistics), блочные
комментарии `/*_*/`, VARBINARY/ENUM/SET, INSERT IGNORE, ON DUPLICATE KEY
UPDATE с арифметикой, составные первичные ключи (mw_l10n_cache), JOIN ON в
любой ориентации, срезание index-hint'ов, нуль-строчные CTE.

## Наблюдения этого прогона (не блокируют установку)

- **Порт для MySQL — внутри `--dbserver`.** MediaWiki принимает `--dbport`,
  но для драйвера `mysql` он игнорируется (`$wgDBport` — только PostgreSQL);
  MySQL-адаптер ждёт `host:port` в `--dbserver`. С `--dbserver 127.0.0.1`
  без порта mysqli стучится в дефолтный 3306 и получает `Connection refused`
  — это не баг DNA-DB, а квирка MW, отражена в драйвере.
- **Агрегаты поверх каталога.** `SELECT COUNT(*) AS c FROM
  information_schema.tables WHERE table_schema = '…'` возвращает строки
  каталога с `c = NULL` вместо одной агрегатной строки: шим каталога
  обрабатывает проекции, но не агрегаты. Живой MediaWiki таких запросов не
  делает (его пробы — `SHOW TABLES LIKE` и точечные SELECT из каталога);
  зафиксировано как кандидат на следующий движковый раунд.
- **Неизвестная колонка в ON/WHERE не даёт ошибку.** Проба со схемой до
  1.39 (`rev_text_id`, удалённый в MCR) вернула пустой результат вместо
  SQLSTATE 42S22. Установка и работа MediaWiki 1.43 этого не касаются;
  кандидат на «строгий режим» вместе с предыдущим пунктом.
