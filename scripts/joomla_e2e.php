<?php
// Joomla 5 web-installer E2E driver against DNA-DB wire server.
// Replicates installation/template/js/setup.js + template.js flow:
//   GET index.php -> dbcheck -> create -> populate1..3 -> custom1,2 -> config
//   -> (remove view) disableAutomatedUpdates -> removeFolder -> site checks
// Usage: php joomla_e2e.php [baseURL] [dbPrefix]
error_reporting(E_ALL & ~E_DEPRECATED);
$BASE = rtrim($argv[1] ?? 'http://127.0.0.1:8080', '/');
$PREFIX = $argv[2] ?? 'jsml_';
$JAR = sys_get_temp_dir() . '/joomla-cookies-' . getmypid() . '.txt';

function fail(string $m): never { fwrite(STDERR, "JOOMLA_FAILED: $m\n"); exit(1); }
function ok(string $m): void { echo "[ok] $m\n"; }

function http(string $method, string $url, ?string $body, string $jar, string $accept = 'application/json'): array {
    static $ch = null;
    if ($ch === null) { $ch = curl_init(); }
    $h = ["Accept: $accept", "X-Requested-With: XMLHttpRequest"];
    if ($body !== null) { $h[] = 'Content-Type: application/x-www-form-urlencoded'; }
    curl_setopt_array($ch, [
        CURLOPT_URL => $url, CURLOPT_CUSTOMREQUEST => $method,
        CURLOPT_RETURNTRANSFER => true, CURLOPT_COOKIEJAR => $jar, CURLOPT_COOKIEFILE => $jar,
        CURLOPT_HTTPHEADER => $h, CURLOPT_TIMEOUT => 120, CURLOPT_FOLLOWLOCATION => true,
    ]);
    if ($body !== null) { curl_setopt($ch, CURLOPT_POSTFIELDS, $body); }
    $resp = curl_exec($ch);
    if ($resp === false) fail("http $url: " . curl_error($ch));
    return [$resp, curl_getinfo($ch, CURLINFO_RESPONSE_CODE)];
}

// ---- 1. GET installer page: session + token + form skeleton ----
[$html, $code] = http('GET', "$BASE/installation/index.php", null, $JAR, 'text/html');
if ($code !== 200 || !str_contains($html, 'jform')) fail("installer page HTTP $code, len " . strlen($html));
if (preg_match('/<input type="hidden" name="([0-9a-f]{32})" value="1"/', $html, $mm)) {
    $tokenName = $mm[1];
} elseif (preg_match('/Joomla\.replaceTokens\("([0-9a-f]{32})"/', $html, $mm)) {
    $tokenName = $mm[1];
} else { fail('CSRF token not found in installer page'); }
ok("installer page loaded, token=$tokenName");

// ---- 2. build the setup form payload (jform array, as serialiseForm would) ----
$jform = [
    'language' => 'en-GB',
    'site_name' => 'DNA-DB Joomla Wire Test',
    'admin_user' => 'DNA-DB Admin',
    'admin_username' => 'dnadmin',
    'admin_email' => 'admin@example.com',
    'admin_password' => 'DnaDb!Joomla2026x',
    'admin_password2' => 'DnaDb!Joomla2026x',
    'db_type' => 'mysqli',
    'db_host' => '127.0.0.1:13306',
    'db_user' => 'root',
    'db_pass' => 'dnatest',
    'db_name' => 'dnajoomla',
    'db_prefix' => $PREFIX,
    'db_old' => 'remove',
    'db_encryption' => '0',
    'db_sslkey' => '',
    'db_sslcert' => '',
    'db_sslverifyservercert' => '0',
    'db_sslca' => '',
    'db_sslcipher' => '',
    'public_folder' => '',
];

function task(string $t, array $jform, string $BASE, string $JAR): array {
    global $tokenName;
    $body = http_build_query(['jform' => $jform]) . '&' . $tokenName . '=1';
    [$resp, $code] = http('POST', "$BASE/installation/index.php?task=installation.$t&format=json", $body, $JAR);
    $j = json_decode($resp, true);
    if (!is_array($j)) fail("task $t: non-JSON (HTTP $code): " . substr($resp, 0, 300));
    if (!empty($j['token']) && preg_match('/^[0-9a-f]{32}$/', $j['token'])) {
        $tokenName = $j['token'];   // Joomla rotates the form token on every JSON response
    }
    return $j;
}

// ---- 3. dbcheck ----
$r = task('dbcheck', $jform, $BASE, $JAR);
if (($r['data']['validated'] ?? false) !== true) {
    fail('dbcheck not validated: ' . json_encode(['err' => $r['error'] ?? null, 'msg' => $r['message'] ?? null, 'messages' => $r['messages'] ?? null]));
}
ok('dbcheck validated (mysqli via DNA-DB wire)');

// ---- 4. create database ----
$r = task('create', $jform, $BASE, $JAR);
if ($r['error'] ?? false) fail('create: ' . json_encode($r['messages'] ?? $r['message'] ?? '?'));
ok('createDatabase + handleOldDatabase OK');

// ---- 5. populate: base, supports, extensions, localise, custom ----
foreach (['populate1' => 'base', 'populate2' => 'supports', 'populate3' => 'extensions', 'custom1' => 'localise', 'custom2' => 'custom'] as $t => $schema) {
    $r = task($t, $jform, $BASE, $JAR);
    if ($r['error'] ?? false) fail("$t ($schema.sql): " . json_encode($r['messages'] ?? $r['message'] ?? '?'));
    ok("populate: $schema.sql loaded");
}

// ---- 6. config (configuration.php + admin user) ----
$r = task('config', $jform, $BASE, $JAR);
if ($r['error'] ?? false) fail('config: ' . json_encode($r['messages'] ?? $r['message'] ?? '?'));
if (($r['data']['view'] ?? '') !== 'remove') fail('config: expected view=remove, got ' . json_encode($r['data'] ?? null));
ok('config: configuration.php written, admin user created');

// ---- 7. remove installation directory (the real remove.js flow) ----
// the UI navigates to the remove VIEW first; the page carries the token
// its JS then posts with (each JSON response rotates it again)
[$html2, $code2] = http('GET', "$BASE/installation/index.php?view=remove", null, $JAR, 'text/html');
if (preg_match('/<input type="hidden" name="([0-9a-f]{32})" value="1"/', $html2, $mm2)) {
    $GLOBALS['tokenName'] = $mm2[1];
} else {
    fail('no token on remove page (HTTP ' . $code2 . ')');
}
$r = task('disableAutomatedUpdates', $jform, $BASE, $JAR);
if ($r['error'] ?? false) fail('disableAutomatedUpdates: ' . json_encode($r['message'] ?? '?'));
ok('automated updates disabled');
$r = task('removeFolder', $jform, $BASE, $JAR);
if ($r['error'] ?? false) fail('removeFolder: ' . json_encode($r['message'] ?? '?'));
ok('installation/ folder removed');

// ---- 8. post-install verification ----
[$fe, $feCode] = http('GET', "$BASE/index.php", null, $JAR, 'text/html');
if ($feCode !== 200) fail("frontend HTTP $feCode, body tail: " . substr(strip_tags($fe), -400));
if (!str_contains($fe, '<html') ) fail('frontend: not HTML');
ok('frontend /index.php renders (HTTP 200)');
[$adm, $admCode] = http('GET', "$BASE/administrator/index.php", null, $JAR, 'text/html');
if ($admCode !== 200 || !str_contains($adm, 'log')) fail("administrator HTTP $admCode");
ok('administrator login page renders (HTTP 200)');

// ---- 9. DB-level checks through the wire (mysqli) ----
$m = @new mysqli('127.0.0.1', 'root', 'dnatest', 'dnajoomla', 13306);
if ($m->connect_errno) fail("mysqli connect: {$m->connect_error}");
$m->set_charset('utf8mb4');
$users = $m->query("SELECT COUNT(*) c FROM {$PREFIX}users")->fetch_assoc()['c'] ?? '0';
if ((int)$users < 1) fail("admin user row missing in {$PREFIX}users (count=$users)");
$arts = $m->query("SELECT COUNT(*) c FROM {$PREFIX}assets")->fetch_assoc()['c'] ?? '0';
$exts = $m->query("SELECT COUNT(*) c FROM {$PREFIX}extensions")->fetch_assoc()['c'] ?? '0';
$artsTbl = $m->query("SELECT COUNT(*) c FROM {$PREFIX}content")->fetch_assoc()['c'] ?? '0';
$cfg = $m->query("SELECT params FROM {$PREFIX}extensions WHERE name = 'com_content' ")->fetch_assoc();
ok("DB: users=$users assets=$arts extensions=$exts content=$artsTbl");

// article INSERT through prepared statement (Joomla-like write path)
$st = $m->prepare("INSERT INTO {$PREFIX}content (asset_id, title, alias, introtext, state,catid, created, created_by, access, language) VALUES (0,?,?,?,?,1,1,1,1,'*')");
if (!$st) fail('prepare article INSERT: ' . $m->error);
$title = 'Hello from DNA-DB'; $alias = 'hello-from-dna-db';
$intro = '<p>Prepared-statement insert via DNA-DB wire protocol.</p>';
$st->bind_param('sssi', $title, $alias, $intro, $catid);
$catid = 9; // uncategorized
if (!$st->execute()) fail('article INSERT: ' . $st->error);
$id = $m->insert_id;
$st->close();
$row = $m->query("SELECT id, title, alias FROM {$PREFIX}content WHERE id = $id")->fetch_assoc();
if (!$row || $row['title'] !== $title) fail('article read-back failed: ' . json_encode($row));
ok("article INSERT/SELECT roundtrip via prepared stmt (id=$id)");
$m->close();

echo "JOOMLA_E2E_OK\n";
@unlink($JAR);
