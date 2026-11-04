<?php
// SPDX-License-Identifier: AGPL-3.0-only
// smoke_wire.php — mysqli + PDO smoke test against DNA-DB's MySQL wire port.
// Usage: php smoke_wire.php [host] [port] [user] [pass]
// Prints SMOKE_OK on success, SMOKE_FAILED:<reason> on any failure.

$host = $argv[1] ?? '127.0.0.1';
$port = (int)($argv[2] ?? 13306);
$user = $argv[3] ?? 'root';
$pass = $argv[4] ?? 'dnatest';

function fail(string $why): never {
    echo "SMOKE_FAILED: $why\n";
    exit(1);
}

// --- 1) wait for the wire port (bounded, no shell /dev/tcp) -----------------
$m = null;
for ($i = 0; $i < 50; $i++) {
    $m = @mysqli_connect($host, $user, $pass, '', $port);
    if ($m) break;
    usleep(200_000);
}
if (!$m) fail('connect: ' . mysqli_connect_error());

$q = fn(string $sql) => mysqli_query($m, $sql)
    or fail("query [$sql]: " . mysqli_error($m));

// --- 2) lifecycle ------------------------------------------------------------
$res = mysqli_query($m, 'SHOW DATABASES');
$dbs = $res ? array_map(fn($r) => $r[0], mysqli_fetch_all($res)) : [];
if (in_array('smoke_wire', $dbs, true)) $q('DROP DATABASE smoke_wire');
$q('CREATE DATABASE smoke_wire');
$q('USE smoke_wire');
$q('CREATE TABLE t (id INT UNSIGNED AUTO_INCREMENT PRIMARY KEY, name VARCHAR(64) NOT NULL, qty INT NOT NULL DEFAULT 0) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4');

// --- 3) prepared INSERT roundtrip -------------------------------------------
$st = mysqli_prepare($m, 'INSERT INTO t (name, qty) VALUES (?, ?)')
    or fail('prepare insert: ' . mysqli_error($m));
$n = 'α-cellsample-§7';
$qty = 42;
mysqli_stmt_bind_param($st, 'si', $n, $qty);
mysqli_stmt_execute($st) or fail('execute insert: ' . mysqli_stmt_error($st));
$id = mysqli_insert_id($m);
if ($id < 1) fail('insert_id=' . $id);
mysqli_stmt_close($st);

// --- 4) prepared SELECT + UPDATE --------------------------------------------
$st = mysqli_prepare($m, 'SELECT name, qty FROM t WHERE id = ?')
    or fail('prepare select: ' . mysqli_error($m));
mysqli_stmt_bind_param($st, 'i', $id);
mysqli_stmt_execute($st) or fail('execute select: ' . mysqli_stmt_error($st));
$row = mysqli_fetch_assoc(mysqli_stmt_get_result($st));
if ($row === null) fail('select: no row');
if ($row['name'] !== $n || (int)$row['qty'] !== 42) fail('roundtrip mismatch: ' . json_encode($row, JSON_UNESCAPED_UNICODE));
mysqli_stmt_close($st);

$q("UPDATE t SET qty = qty + 8 WHERE id = $id");
$r = mysqli_query($m, "SELECT qty FROM t WHERE id = $id");
$v = (int)mysqli_fetch_row($r)[0];
if ($v !== 50) fail("update: qty=$v");

// --- 5) PDO path + aggregates ------------------------------------------------
$pdo = new PDO("mysql:host=$host;port=$port;dbname=smoke_wire;charset=utf8mb4", $user, $pass, [
    PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION,
]);
$cnt = (int)$pdo->query('SELECT COUNT(*) FROM t')->fetchColumn();
if ($cnt !== 1) fail("pdo count=$cnt");
$sum = (int)$pdo->query('SELECT SUM(qty) FROM t')->fetchColumn();
if ($sum !== 50) fail("pdo sum=$sum");

// --- 6) session state + catalog ----------------------------------------------
$dbs = [];
$res = mysqli_query($m, 'SHOW DATABASES');
while ($r = mysqli_fetch_row($res)) $dbs[] = $r[0];
if (!in_array('smoke_wire', $dbs, true)) fail('SHOW DATABASES missing smoke_wire');
// Handshake reports the MariaDB-compat string on purpose (CMS drivers
// branch on it); DNA-DB identity comes through @@version_comment.
$ver = mysqli_get_server_info($m);
if ($ver === '' || stripos($ver, 'MariaDB') === false) fail("server info: $ver");
$vc = mysqli_fetch_row(mysqli_query($m, 'SELECT @@version_comment'))[0] ?? '';
if (stripos($vc, 'DNA-DB') === false) fail("version_comment: $vc");

mysqli_close($m);
echo "SMOKE_OK server=$ver id=$id\n";
