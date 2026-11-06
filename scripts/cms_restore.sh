#!/bin/bash
# Операторский рестор CMS-базы в DNA-DB (Фаза 3 «эксплуатация»).
#
# Проигрывает логический бэкаг (артефакт cms_backup.sh / mariadb-dump) в
# DNA-DB через штатный клиент mariadb. Гейт сам разворачивает условные
# комментарии `/*!…*/` и `/*M!…*/` (включая sandbox-маркер mariadb-dump
# ≥ 10.11.18), отвечает OK на LOCK/UNLOCK TABLES и DISABLE/ENABLE KEYS.
# Повторный запуск безопасен: дамп содержит DROP TABLE IF EXISTS
# (идемпотентность проверена в E2E).
#
# Использование:
#   cms_restore.sh <host> <port> <user> <pass> <db> <backupfile> [--create]
#
#   --create  создать базу, если её нет (после дисастера)
#
# Переменные: MR — basedir с бинарями MariaDB (клиент ищется в $MR/bin,
# затем в PATH).
set -euo pipefail
HOST=${1:?host}; PORT=${2:?port}; USER=${3:?user}; PASS=${4:?pass}
DB=${5:?db}; FILE=${6:?backupfile}
CREATE=0
[ "${7:-}" = "--create" ] && CREATE=1
MR=${MR:-/home/z/my-project/runtime/mariadb}

CLI="$MR/bin/mariadb"
[ -x "$CLI" ] || CLI="$(command -v mariadb || command -v mysql)"
export LD_LIBRARY_PATH="$MR/lib:${LD_LIBRARY_PATH:-}"

[ -s "$FILE" ] || { echo "RESTORE_FAILED: backup '$FILE' missing or empty"; exit 1; }

if [ "$CREATE" = 1 ]; then
  "$CLI" --no-defaults -h "$HOST" -P "$PORT" -u "$USER" -p"$PASS" \
    -e "CREATE DATABASE IF NOT EXISTS \`$DB\`"
fi

"$CLI" --no-defaults -h "$HOST" -P "$PORT" -u "$USER" -p"$PASS" "$DB" < "$FILE"

TABLES=$("$CLI" --no-defaults -h "$HOST" -P "$PORT" -u "$USER" -p"$PASS" -N \
  -e "SHOW TABLES IN \`$DB\`" 2>/dev/null | wc -l)
echo "RESTORE_OK db=$DB tables=$TABLES source=$FILE"
