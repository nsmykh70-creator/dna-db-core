# CMS wire-бенчмарк: DNA-DB mysqld vs MariaDB

- Дата: 2026-11-02 17:57:44, платформа: `Linux-5.10.134-013.15.kangaroo.al8.x86_64-x86_64-with-glibc2.41`
- Объём: 8 000 строк, одинаковый SQL через pymysql (text protocol, как wpdb)
- Версии: DNA-DB CMS Edition (`10.11.11-MariaDB` — wire-совместимая строка версии), MariaDB `11.8.6-MariaDB`
- Методика: одинаковый SQL через pymysql (text protocol, как wpdb); ops/s = строк/сек по фазе; RSS = пик VmRSS демона за фазу (сэмплер 50 мс + синхронный baseline, замер строго своего процесса)

| Фаза | DNA-DB ops/s | MariaDB ops/s | DNA × MariaDB |
|---|---|---|---|
| insert_single | 15 682 | 13 092 | ×1.20 |
| insert_multirow | 125 083 | 216 268 | ×0.58 |
| select_pk | 8 046 | 8 889 | ×0.91 |
| select_index | 8 647 | 8 290 | ×1.04 |
| update_pk | 11 731 | 12 611 | ×0.93 |
| wp_mix | 5 385 | 4 701 | ×1.15 |

| Фаза | RSS DNA-DB, МБ | RSS MariaDB, МБ |
|---|---|---|
| insert_single | 8 | 118 |
| insert_multirow | 11 | 118 |
| select_pk | 13 | 119 |
| select_index | 13 | 119 |
| update_pk | 13 | 122 |
| wp_mix | 13 | 123 |
