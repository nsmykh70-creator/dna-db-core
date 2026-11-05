# DNA-DB CMS Edition — E2E: Drupal 11.4.10 через универсальный MySQL wire-адаптер

Дата: 5 ноября 2026. Ветка: `cms-edition`. Версия движка: v4.0.3 (prebuilt,
пересобран с фиксом compound-ON outer joins; sha256
`33eebc42…3cb067` в `prebuilt/linux-x86_64/SHA256SUMS`).

## Что проверяется

Drupal 11.4.10 ставится на DNA-DB **как на обычный MySQL** штатным
web-установщиком (`core/install.php`): PHP built-in server (статический PHP
8.3.28) → драйвер `mysql` (PDO MySQL, `EMULATE_PREPARES = TRUE`) →
`dna-db-core mysqld :13307`. Профиль `standard`, база `drupal11`, префикс
`dp_`. Установщик ведётся драйвером `scripts/drupal_e2e.php` (общий
« walker » форм: язык → профиль → БД → batch → конфигурация сайта →
редирект на сайт), дальше — homepage, админ-сессия и проверки уровня БД.

## Результат: PASS

| Проверка | Результат |
| --- | --- |
| Форма языка (`en`) + профиля (`standard`) | OK |
| Database settings (`mysql → 127.0.0.1:13307/drupal11`) | OK, Drupal сам подключается через PDO |
| Установочный batch (`op=start → do_nojs → op=finished`) | OK — весь standard profile |
| Site configuration (site name, admin, timezone) | OK |
| Редирект на `/` после установки | HTTP 200, имя сайта «DNA-DB Drupal Wire» |
| Админ-сессия (`/user/login` → 302 → `/user/1` с «Log out») | OK — bcrypt-проверка пароля через wire |
| Таблиц `dp_*` в `drupal11` | 54 |
| `dp_config` / `dp_menu_tree` / `dp_key_value` / `dp_watchdog` | 137 / 65 / 315 / 49 строк |
| `dp_users_field_data` uid=1 = `dnadmin`, `created`/`default_langcode` НЕ потеряны | OK |
| Prepared SELECT (`WHERE uid = ?`) через wire | OK |

## Движковый фикс раунда: compound-ON outer joins (1 фикс + 3 регресс-теста)

Drupal загружает сущности запросом вида

```sql
SELECT "users_field_data".*, "users_field_data"."langcode" AS …
FROM "dp_users_field_data" "users_field_data"
LEFT OUTER JOIN "dp_user__user_picture" "user__user_picture"
  ON "user__user_picture"."entity_id" = "users_field_data"."uid"
 AND "user__user_picture"."langcode" = "users_field_data"."langcode"
 AND "user__user_picture"."deleted" = 0
WHERE "users_field_data"."uid" IN ('1')
```

Старый парсер оставлял от составного ON только первую пару `col = col`
(хэш-join), а остальные конъюнкты **складывал в WHERE**. Для INNER JOIN это
эквивалентно, для LEFT/RIGHT/FULL — нет: конъюнкты ON должны ограничивать
совпадение, а не отфильтровывать NULL-дополненные строки левой стороны.
В итоге нагрузка строки пользователя без картинки пропадала, инсталлятор
пересохранял uid=1 с `created/changed/default_langcode = NULL`, и админский
логин (`… WHERE name = 'dnadmin' AND default_langcode = 1`) находил 0 строк
→ «Unrecognized username or password».

Фикс:
- `JoinSpec.on_extra` — конъюнкты составного ON едут вместе с join'ом;
  верхнеуровневая цепочка `AND` разворачивается в отдельные конъюнкты;
- INNER: фильтр результата сразу после join'а (эквивалент старого пути);
- LEFT/RIGHT: `hash_join_left_extras` — пара считается совпавшей только если
  equi-пара совпала **и** все конъюнкты TRUE на слитой строке; левые строки
  без совпадения сохраняются с NULL-колонками правой таблицы;
- FULL: `hash_join_full_extras` — та же логика с дополнением обеих сторон.

Регресс-тесты (`src/sql_engine/tests/battery/tests/indexes_joins.rs`):
`left_join_compound_on_keeps_unmatched_left_rows`,
`left_join_compound_on_matches_when_every_conjunct_holds`,
`inner_join_compound_on_filters_pairs`. Полный набор: **285 lib-тестов
зелёные** (`cargo test --features cms`).

## Воспроизведение

```bash
dna-db-core mysqld --data-dir /tmp/drupal_dna_data --bind 127.0.0.1 \
  --port 13307 --user root:dnatest &
chmod -R a+w drupal-11.4.10/sites/default   # установщик пишет settings.php
php -S 127.0.0.1:8088 -t drupal-11.4.10 &
php scripts/drupal_e2e.php http://127.0.0.1:8088 13307
# ожидание: DRUPAL_E2E_OK
```

Обёртка: `scripts/e2e_drupal_mysqlwire.sh` (старт сервера, ожидание порта,
права на `sites/default`, web-server, драйвер, вердикт — одним вызовом).

## Наблюдения этого прогона (не блокируют установку)

- **User-Agent обязателен.** `install_begin_request()` делает
  `str_contains($user_agent, 'simpletest')`; PHP-curl по умолчанию НЕ шлёт
  `User-Agent`, `HTTP_USER_AGENT` = NULL, депрекейшен PHP 8.3 превращается
  установщиком в фатальную страницу. Драйвер шлёт явный UA — это квирка
  Drupal + PHP-curl, не DNA-DB.
- **`SHOW CREATE TABLE` не поддержан** (ошибка «unsupported SHOW target»).
  Инсталляторы его не используют; кандидат на следующий раунд каталога.
- **Порт — отдельным полем.** В отличие от MediaWiki, Drupal принимает порт
  штатным полем формы (`mysql[advanced_options][port]`) — универсальный
  адаптер принимает оба варианта.
- Имена полей формы БД в Drupal 11 неймспейсируются классом драйвера
  (`Drupal\mysql\Driver\Database\mysql[database]`) — драйвер E2E
  сериализует их так же, как браузер (вложенные массивы).
