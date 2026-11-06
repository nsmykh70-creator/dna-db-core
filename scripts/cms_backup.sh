#!/bin/bash
# Операторский бэкап CMS-базы с DNA-DB (Фаза 3 «эксплуатация»).
#
# Снимает логический бэкаг штатным mariadb-dump: DNA-DB отвечает на
# SHOW CREATE TABLE / SELECT SQL_NO_CACHE / LOCK TABLES как MySQL, поэтому
# дампер работает без доработок. Артефакт восстанавливается и в DNA-DB,
# и в ванильный MySQL/MariaDB (см. docs/cms/E2E_BACKUP_RESTORE_REPORT.md).
#
# Использование:
#   cms_backup.sh <host> <port> <user> <pass> <db> [outfile]
#
# Переменные: MR — basedir с бинарями MariaDB (mariadb-dump ищется в
# $MR/bin, затем в PATH).
set -euo pipefail
HOST=${1:?host}; PORT=${2:?port}; USER=${3:?user}; PASS=${4:?pass}
DB=${5:?db}; OUT=${6:-"${DB}_backup_$(date +%Y%m%d_%H%M%S).sql"}
MR=${MR:-/home/z/my-project/runtime/mariadb}

DUMP="$MR/bin/mariadb-dump"
[ -x "$DUMP" ] || DUMP="$(command -v mariadb-dump || command -v mysqldump)"
export LD_LIBRARY_PATH="$MR/lib:${LD_LIBRARY_PATH:-}"

# NB: флаги сознательно минимальны — в точности как в проверенном E2E
# (scripts/e2e_backup_restore.sh). --single-transaction через гейт пока
# не проверялся; непротиворечивость на одиночном писателе даёт сам движок.
"$DUMP" --no-defaults -h "$HOST" -P "$PORT" -u "$USER" -p"$PASS" "$DB" > "$OUT"

TABLES=$(grep -c '^CREATE TABLE' "$OUT" || true)
echo "BACKUP_OK file=$OUT bytes=$(wc -c < "$OUT") tables=$TABLES"
