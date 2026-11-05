<?php
// Wire-level DECIMAL(p,s) smoke: fixed-scale strings + NEWDECIMAL metadata
// through both COM_QUERY (text) and COM_STMT_EXECUTE (binary) paths.
error_reporting(E_ALL & ~E_DEPRECATED);
$m = mysqli_init();
$m->real_connect('127.0.0.1', 'wp', 'wppass', 'wordpress', 13307) or die("connect\n");

$m->query('DROP TABLE IF EXISTS t_dec');
$m->query('CREATE TABLE t_dec (id bigint unsigned NOT NULL AUTO_INCREMENT PRIMARY KEY,
    label varchar(30) DEFAULT \'\', total decimal(10,2) NOT NULL DEFAULT \'0.00\', ratio decimal(12,8) DEFAULT NULL)');

$ins = $m->prepare('INSERT INTO t_dec (label, total, ratio) VALUES (?, ?, ?)');
$a = 'a'; $b = '250'; $c = 0.125;
$ins->bind_param('ssd', $a, $b, $c);
$ins->execute();
$b2 = 0; // int into decimal(10,2)
$ins->bind_param('ssd', $a, $b2, $c);
$ins->execute();
$ins->close();

// --- text protocol values ---
$r = $m->query('SELECT id, label, total, ratio FROM t_dec ORDER BY id');
$row1 = $r->fetch_assoc();
$row2 = $r->fetch_assoc();
$ok = true;
$ok &= ($row1['total'] === '250.00');
$ok &= ($row1['ratio'] === '0.12500000');
$ok &= ($row2['total'] === '0.00');
echo "text values: total={$row1['total']} ratio={$row1['ratio']} zero={$row2['total']}\n";

// --- text protocol metadata (mysqli result meta) ---
$r = $m->query('SELECT total, ratio, label FROM t_dec');
while ($col = $r->fetch_field()) {
    // MYSQL_TYPE_NEWDECIMAL = 246
    echo "meta text: {$col->name} type={$col->type} decimals={$col->decimals}\n";
    if ($col->name === 'total') { $ok &= ($col->type == 246 && $col->decimals == 2); }
    if ($col->name === 'ratio') { $ok &= ($col->type == 246 && $col->decimals == 8); }
    if ($col->name === 'label') { $ok &= ($col->type == 253); }
}
$r->close();

// --- prepared (binary) protocol: metadata + values ---
$st = $m->prepare('SELECT total, ratio FROM t_dec WHERE id = ?');
$id = 1;
$st->bind_param('i', $id);
$st->execute();
$meta = $st->result_metadata();
while ($col = $meta->fetch_field()) {
    echo "meta bin: {$col->name} type={$col->type} decimals={$col->decimals}\n";
    if ($col->name === 'total') { $ok &= ($col->type == 246 && $col->decimals == 2); }
}
$st->bind_result($total, $ratio);
$st->fetch();
$st->close();
echo "bin values: total=$total ratio=$ratio\n";
$ok &= ($total === '250.00' && $ratio === '0.12500000');

// --- SUM keeps scale; WHERE compares numerically ---
$r = $m->query('SELECT SUM(total) AS total FROM t_dec');
$s = $r->fetch_assoc()['total'];
echo "sum: $s\n";
$ok &= ($s === '250.00');
$r = $m->query("SELECT COUNT(*) AS c FROM t_dec WHERE total > 100");
$ok &= ($r->fetch_assoc()['c'] == 1);

echo $ok ? "DECIMAL_WIRE_OK\n" : "DECIMAL_WIRE_FAILED\n";
exit($ok ? 0 : 1);
