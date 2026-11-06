<?php
// SPDX-License-Identifier: AGPL-3.0-only
// CMS restore smoke: a WordPress-flavoured read path against a freshly
// restored database. Proves the restored data serves CMS-shaped queries
// (counts + keyed meta lookup) through mysqli — the stack every PHP CMS
// uses. Usage:
//   cms_restore_smoke.php <host> <port> <db> <user> <pass>
// Verdict: RESTORE_SMOKE_OK / RESTORE_SMOKE_FAILED.

mysqli_report(MYSQLI_REPORT_ERROR | MYSQLI_REPORT_STRICT);
[$host, $port, $db, $user, $pass] = array_slice($argv, 1);
$c = new mysqli($host, $user, $pass, $db, (int)$port);
$c->set_charset('utf8mb4');

$posts = (int)$c->query("SELECT COUNT(*) FROM `wp_posts`")->fetch_row()[0];
$meta  = (int)$c->query("SELECT COUNT(*) FROM `wp_postmeta`")->fetch_row()[0];
$r = $c->query("SELECT `meta_value` FROM `wp_postmeta` WHERE `meta_key` = '_thumbnail_id' AND `post_id` = 1");
$v = $r->fetch_row()[0] ?? '';
$r = $c->query("SELECT `post_title` FROM `wp_posts` WHERE `post_status` = 'trash'");
$trash = $r->fetch_row()[0] ?? '';

$ok = $posts === 4 && $meta === 4 && $v === '42' && $trash === 'NULL-heavy row';
echo $ok
    ? "RESTORE_SMOKE_OK posts=$posts meta=$meta thumbnail=$v\n"
    : "RESTORE_SMOKE_FAILED posts=$posts meta=$meta thumbnail=$v trash=$trash\n";
exit($ok ? 0 : 1);
