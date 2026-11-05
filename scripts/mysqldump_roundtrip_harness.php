<?php
// SPDX-License-Identifier: AGPL-3.0-only
// mysqldump roundtrip harness: fixture creation on the reference MariaDB and
// content comparison between two servers (reference vs DNA-DB, or reference
// vs re-imported-back). Modes:
//   fixture <host> <port> <db>                                  — create schema+data
//   compare <h1> <p1> <db1> <h2> <p2> <db2> <user2> <pass2>     — diff contents
//   probe   <host> <port> <db> <user> <pass>                    — auto-inc continuity
// Verdict lines: FIXTURE_OK / FIXTURE_FAILED, per-table HASH_OK|HASH_DIFF,
// COMPARE_MATCH|COMPARE_DIFF, PROBE_OK|PROBE_FAILED.

$mode = $argv[1] ?? '';
array_shift($argv); // drop the script name — destructurings below start at 'mode'
mysqli_report(MYSQLI_REPORT_ERROR | MYSQLI_REPORT_STRICT);

function conn(string $host, int $port, string $db, string $user = 'root', string $pass = ''): mysqli {
    $tries = 60;
    for ($i = 0; $i < $tries; $i++) {
        try {
            $c = new mysqli($host, $user, $pass, $db, $port);
            // Without this MariaDB converts results to the client's default
            // (latin1) and non-ASCII comes back as '?' — a compare artifact,
            // not a data difference.
            $c->set_charset('utf8mb4');
            return $c;
        } catch (mysqli_sql_exception $e) {
            if ($i === $tries - 1) throw $e;
            usleep(500_000);
        }
    }
    throw new RuntimeException('unreachable');
}

/** Canonical dump of a table's contents: rows sorted, values \x1f-joined. */
function table_hash(mysqli $c, string $table): array {
    $cols = [];
    $r = $c->query("SHOW COLUMNS FROM `$table`");
    while ($row = $r->fetch_assoc()) { $cols[] = $row['Field']; }
    $r = $c->query("SELECT * FROM `$table`");
    $rows = [];
    while ($row = $r->fetch_row()) {
        $vals = [];
        foreach ($row as $v) { $vals[] = ($v === null) ? "\x00N" : (string)$v; }
        $rows[] = implode("\x1f", $vals);
    }
    sort($rows, SORT_STRING);
    return [$cols, $rows];
}

if ($mode === 'fixture') {
    [$_, $host, $port, $db] = $argv;
    $c = conn($host, (int)$port, $db);
    $c->query("SET NAMES utf8mb4");
    $c->query("DROP TABLE IF EXISTS wp_posts, wp_postmeta, wp_term_rel");
    $c->query(<<<'SQL'
CREATE TABLE `wp_posts` (
  `ID` bigint(20) unsigned NOT NULL AUTO_INCREMENT,
  `post_author` bigint(20) unsigned NOT NULL DEFAULT '0',
  `post_title` text NOT NULL,
  `post_name` varchar(200) NOT NULL DEFAULT '',
  `post_content` longtext,
  `post_status` varchar(20) NOT NULL DEFAULT 'publish',
  `post_date` datetime NOT NULL DEFAULT '1970-01-01 00:00:00',
  `post_price` decimal(10,2) NOT NULL DEFAULT '0.00',
  `post_views` int(10) unsigned DEFAULT NULL,
  `post_blob` blob,
  `post_flags` tinyint(1) NOT NULL DEFAULT '0',
  PRIMARY KEY (`ID`),
  KEY `post_name` (`post_name`(191)),
  KEY `type_status_date` (`post_status`,`post_date`,`ID`)
) ENGINE=InnoDB AUTO_INCREMENT=100 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci
SQL);
    $c->query(<<<'SQL'
CREATE TABLE `wp_postmeta` (
  `meta_id` bigint(20) unsigned NOT NULL AUTO_INCREMENT,
  `post_id` bigint(20) unsigned NOT NULL DEFAULT '0',
  `meta_key` varchar(255) DEFAULT NULL,
  `meta_value` longtext,
  PRIMARY KEY (`meta_id`),
  KEY `post_id` (`post_id`),
  KEY `meta_key` (`meta_key`(191))
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci
SQL);
    $c->query(<<<'SQL'
CREATE TABLE `wp_term_rel` (
  `object_id` bigint(20) unsigned NOT NULL DEFAULT '0',
  `term_taxonomy_id` bigint(20) unsigned NOT NULL DEFAULT '0',
  `term_order` int(11) NOT NULL DEFAULT '0',
  PRIMARY KEY (`object_id`,`term_taxonomy_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci
SQL);

    // Tricky values: unicode/emoji, quotes, backslashes, newlines, tabs,
    // NULLs, decimals, ids with gaps (auto-inc bump check), binary blob.
    $st = $c->prepare("INSERT INTO `wp_posts` (`ID`,`post_author`,`post_title`,`post_name`,`post_content`,`post_status`,`post_date`,`post_price`,`post_views`,`post_blob`,`post_flags`) VALUES (?,?,?,?,?,?,?,?,?,?,?)");
    $rows = [
        [1,  1, "Просто заголовок", "prosto-zagolovok", "Текст с \"кавычками\" и 'апострофами'", "publish", "2026-01-15 10:30:00", "250.00", 100, "\x00\x27\x01\\backslash", 1],
        [7,  2, "Emoji 😀🚀 title", "emoji-title", "Line1\nLine2\tTabbed\r\nCRLF", "draft", "2026-02-28 23:59:59", "0.10", null, null, 0],
        [42, 1, "Back\\slash \\w combo", "backslash", "SELECT * FROM t WHERE s = 'a\\'b' -- inline", "publish", "2026-03-01 00:00:00", "99999999.99", 4294967295, "bin\x01\x02data", 1],
        [99, 3, "NULL-heavy row", "null-heavy", null, "trash", "2024-12-31 12:00:00", "13.37", null, "\xe2\x82\xacuro", 0],
    ];
    foreach ($rows as $r) {
        $id=$r[0];$auth=$r[1];$title=$r[2];$name=$r[3];$content=$r[4];$status=$r[5];$date=$r[6];$price=$r[7];$views=$r[8];$blob=$r[9];$flags=$r[10];
        $blobS = $blob === null ? null : $blob;
        $st->bind_param("iissssssisi", $id, $auth, $title, $name, $content, $status, $date, $price, $views, $blobS, $flags);
        $st->execute();
    }
    $st->close();

    $st = $c->prepare("INSERT INTO `wp_postmeta` (`meta_id`,`post_id`,`meta_key`,`meta_value`) VALUES (?,?,?,?)");
    $meta = [
        [1, 1, "_thumbnail_id", "42"],
        [2, 1, "price_history", "a:2:{i:0;s:6:\"250.00\";i:1;s:6:\"249.00\";}"],
        [3, 7, null, "NULL key row"],
        [4, 42, "unicode_ключ", "значение✔"],
    ];
    foreach ($meta as $m) {
        $id=$m[0];$pid=$m[1];$k=$m[2];$v=$m[3];
        $st->bind_param("iiss", $id, $pid, $k, $v);
        $st->execute();
    }
    $st->close();
    $c->query("INSERT INTO `wp_term_rel` (`object_id`,`term_taxonomy_id`,`term_order`) VALUES (1,1,0),(1,2,1),(42,1,0),(7,3,2)");
    echo "FIXTURE_OK tables=3 posts=4 meta=4 rel=4\n";
    $c->close();
    exit(0);
}

if ($mode === 'compare') {
    // compare <h1> <p1> <db1> <h2> <p2> <db2> <user2> <pass2>
    [$_, $h1, $p1, $db1, $h2, $p2, $db2, $u2, $pw2] = $argv;
    $a = conn($h1, (int)$p1, $db1);
    $b = conn($h2, (int)$p2, $db2, $u2, $pw2);
    $ta = []; $r = $a->query("SHOW TABLES");
    while ($row = $r->fetch_row()) { $ta[] = $row[0]; }
    $tb = []; $r = $b->query("SHOW TABLES");
    while ($row = $r->fetch_row()) { $tb[] = $row[0]; }
    sort($ta); sort($tb);
    if ($ta !== $tb) {
        echo "TABLE_LIST_DIFF ref=" . implode(",", $ta) . " dna=" . implode(",", $tb) . "\n";
        echo "COMPARE_DIFF\n"; exit(1);
    }
    $diffs = 0;
    foreach ($ta as $t) {
        [$ca, $ra] = table_hash($a, $t);
        [$cb, $rb] = table_hash($b, $t);
        if ($ca !== $cb) {
            echo "COLUMNS_DIFF $t ref=[" . implode(",", $ca) . "] dna=[" . implode(",", $cb) . "]\n";
            $diffs++; continue;
        }
        if ($ra !== $rb) {
            echo "ROWS_DIFF $t ref_rows=" . count($ra) . " dna_rows=" . count($rb) . "\n";
            for ($i = 0; $i < max(count($ra), count($rb)); $i++) {
                $x = $ra[$i] ?? "(absent)"; $y = $rb[$i] ?? "(absent)";
                if ($x !== $y) {
                    echo "  first_diff_row#$i\n    ref=" . bin2hex(mb_substr($x, 0, 200)) . "\n    dna=" . bin2hex(mb_substr($y, 0, 200)) . "\n";
                    break;
                }
            }
            $diffs++; continue;
        }
        echo "HASH_OK $t rows=" . count($ra) . "\n";
    }
    echo $diffs === 0 ? "COMPARE_MATCH tables=" . count($ta) . "\n" : "COMPARE_DIFF diffs=$diffs\n";
    exit($diffs === 0 ? 0 : 1);
}

if ($mode === 'probe') {
    // probe <host> <port> <db> <user> <pass> — auto-inc continuity after import.
    [$_, $host, $port, $db, $user, $pass] = $argv;
    $c = conn($host, (int)$port, $db, $user, $pass);
    $c->query("INSERT INTO `wp_posts` (`post_author`,`post_title`,`post_name`,`post_status`) VALUES (9,'After import','after-import','publish')");
    $id = $c->insert_id;
    $expected = 100; // highest imported id = 99
    echo "generated_id=$id\n";
    echo $id >= $expected ? "PROBE_OK id=$id\n" : "PROBE_FAILED id=$id expected>=$expected\n";
    // And the LAST_INSERT_ID() function form (wpdb reads it this way).
    $r = $c->query("SELECT LAST_INSERT_ID()");
    $v = (int)$r->fetch_row()[0];
    echo $v === $id ? "LID_OK\n" : "LID_FAILED lid=$v id=$id\n";
    exit(($id >= $expected && $v === $id) ? 0 : 1);
}

fwrite(STDERR, "unknown mode: $mode\n");
exit(2);
