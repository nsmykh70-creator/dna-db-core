| Фаза | DNA-DB ops/s | MariaDB ops/s | DNA × MariaDB |
|---|---|---|---|
| insert_single | 18 043 | 14 799 | ×1.22 |
| insert_multirow | 172 028 | 237 882 | ×0.72 |
| select_pk | 9 429 | 10 177 | ×0.93 |
| select_index | 10 031 | 9 683 | ×1.04 |
| update_pk | 14 434 | 14 853 | ×0.97 |
| wp_mix | 6 388 | 5 306 | ×1.20 |

| Фаза | RSS DNA-DB, МБ | RSS MariaDB, МБ |
|---|---|---|
| insert_single | 188 | 191 |
| insert_multirow | 0 | 191 |
| select_pk | 188 | 192 |
| select_index | 188 | 192 |
| update_pk | 188 | 194 |
| wp_mix | 188 | 195 |
