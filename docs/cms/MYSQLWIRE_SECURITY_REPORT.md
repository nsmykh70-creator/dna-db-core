# DNA-DB CMS Edition — security-отчёт MySQL wire-адаптера

Дата: 2 ноября 2026. Ветка: `cms-edition`. Скрипт:
`scripts/mysqlwire_security_test.py` (запуск: `python3
scripts/mysqlwire_security_test.py <port> <user> <pass>`).

## Сводка: 24 / 24 PASS

| # | Проверка | Результат |
|---|---|---|
| 1 | auth: неверный пароль → errno 1045 | PASS |
| 2 | auth: неизвестный пользователь → errno 1045 | PASS |
| 3 | auth: мусорный 20-байтный токен → ERR 28000 | PASS |
| 4 | auth: пустой токен против пользователя с паролем → ERR | PASS |
| 5 | auth: 20 попыток brute force — все отклонены | PASS |
| 6 | injection: `'; DROP TABLE sec_accounts; --` через параметр → сохранён как литерал | PASS |
| 7 | injection: таблица цела после DROP-попытки | PASS |
| 8 | injection: `OR '1'='1` через параметр не расширяет выборку | PASS |
| 9 | injection: LIKE-wildcard payload не выходит за строковый литерал | PASS |
| 10 | injection: экранированная кавычка `admin\'; …` → 0 строк | PASS |
| 11 | injection: таблица на месте после всех payload'ов | PASS |
| 12 | injection: `admin' UNION SELECT 'hacked' -- ` через параметр → 0 строк | PASS |
| 13 | stacked queries без `CLIENT_MULTI_STATEMENTS`: второй оператор не выполняется (как в MySQL) | PASS |
| 14 | malformed: пустой handshake-ответ → ERR/закрытие | PASS |
| 15 | malformed: мусорные байты → соединение сброшено | PASS |
| 16 | malformed: zero-length пакет → сброс, демон жив | PASS |
| 17 | malformed: усечённый пакет (заявлено 100, послано 10) → сброс, демон жив | PASS |
| 18 | malformed: неверный sequence number → ответ/закрытие | PASS |
| 19 | malformed: демон жив после всего malformed-набора | PASS |
| 20 | errno: 1146 missing table | PASS |
| 21 | errno: 1054 bad column | PASS |
| 22 | errno: 1062 duplicate | PASS |
| 23 | liveness: данные целы после всего набора атак (3 строки: admin, bob, payload-строка) | PASS |
| 24 | liveness: свежее соединение работает | PASS |

## Методика

* SQL-инъекции подаются **параметрами pymysql** (как из wpdb `$wpdb->prepare`)
  — значения едут отдельными полями протокола и обязаны храниться литералами;
  проверяется, что payload не «выпрыгивает» из строкового контекста.
* Auth-векторы шлются сырым протоколом (handshake v10 + скрэмбл
  mysql_native_password) с подменой токена.
* Malformed-пакеты — обрывы на каждом этапе: пустой ответ, нулевая длина,
  расхождение длины заголовка с телом, мусор, неверный seq. Критерий: сервер
  отвечает ERR либо закрывает соединение и **продолжает обслуживать новых
  клиентов**; данные не затрагиваются.
* errno-проброс нужен CMS: плагины различают 1146/1054/1062 для ветвления
  логики.

## Ограничения

* Транспортный TLS для wire-порта в этом прогоне не включался (как у типового
  MySQL на localhost); в продакшене используйте SSH-туннель/файрвол или
  включайте TLS-слушатели DNA-DB (см. docs/SECURITY_RU.md).
* Native security battery DNA-DB (`scripts/security_probe.py` — TLS, Argon2id,
  lockout, pre-auth frame cap) покрывает native-протокол и продолжает
  действовать для движка; wire-слой использует те же storage/auth-примитивы.
