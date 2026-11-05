<?php
// Drupal 11 web-installer E2E driver against DNA-DB wire server.
// Drives the REAL core/install.php over HTTP (PHP built-in server):
//   language -> requirements -> database settings -> install batch ->
//   site configuration -> batch finish -> homepage -> admin login.
// Generic form walker: on every page it parses the main <form>, fills
// defaults, applies known overrides (DB config / site config) and submits
// with the primary op button; batch steps are followed via meta refresh.
// Usage: php drupal_e2e.php [baseURL]
error_reporting(E_ALL & ~E_DEPRECATED);
$BASE = rtrim($argv[1] ?? 'http://127.0.0.1:8088', '/');
$DBPORT = (int)($argv[2] ?? 13306);
$JAR = sys_get_temp_dir() . '/drupal-cookies-' . getmypid() . '.txt';
$GLOBALS['JAR'] = $JAR;
$GLOBALS['DBPORT'] = $DBPORT;

const DB_NAME = 'drupal11';
const DB_USER = 'root';
const DB_PASS = 'dnatest';
const SITE_NAME = 'DNA-DB Drupal Wire';
const ADMIN_NAME = 'dnadmin';
const ADMIN_MAIL = 'admin@example.com';
const ADMIN_PASS = 'DnaDb!Drupal2026x';

function fail(string $m): never { fwrite(STDERR, "DRUPAL_FAILED: $m\n"); exit(1); }
function ok(string $m): void { echo "[ok] $m\n"; }

function http(string $url, ?string $body = null): array {
    static $ch = null;
    if ($ch === null) { $ch = curl_init(); $GLOBALS['CH'] = $ch; }
    // UA is mandatory: install_begin_request() str_contains() on HTTP_USER_AGENT
    // turns a deprecation (null haystack) into a fatal error page otherwise.
    $h = ['Accept: text/html', 'User-Agent: DNA-DB-E2E-Walker/1.0'];
    if ($body !== null) { $h[] = 'Content-Type: application/x-www-form-urlencoded'; }
    curl_setopt_array($ch, [
        CURLOPT_URL => $url, CURLOPT_RETURNTRANSFER => true,
        CURLOPT_COOKIEJAR => $GLOBALS['JAR'], CURLOPT_COOKIEFILE => $GLOBALS['JAR'],
        CURLOPT_HTTPHEADER => $h, CURLOPT_TIMEOUT => 90, CURLOPT_FOLLOWLOCATION => false,
    ]);
    if ($body !== null) { curl_setopt($ch, CURLOPT_POSTFIELDS, $body); }
    $resp = curl_exec($ch);
    if ($resp === false) fail("http $url: " . curl_error($ch));
    return [$resp, curl_getinfo($ch, CURLINFO_RESPONSE_CODE)];
}

function redirect_url(): string {
    return (string)curl_getinfo($GLOBALS['CH'], CURLINFO_REDIRECT_URL);
}

/** GET with PHP-built-in-server fallback: /user/1 -> /index.php/user/1 on 404. */
function get_page(string $url): array {
    [$html, $code] = http($url);
    $p = parse_url($url, PHP_URL_PATH) ?? '/';
    if ($code === 404 && !str_contains($p, '.php') && $p !== '/') {
        $q = parse_url($url, PHP_URL_QUERY);
        [$html, $code] = http($GLOBALS['BASE'] . '/index.php' . $p . ($q ? "?$q" : ''));
    }
    return [$html, $code];
}

function abs_url(string $u): string {
    global $BASE;
    $u = html_entity_decode($u);
    $u = trim($u, "'\"");
    if ($u === '') return $BASE . '/';
    if (str_starts_with($u, 'http://') || str_starts_with($u, 'https://')) return $u;
    if (str_starts_with($u, '?')) { $p = parse_url($GLOBALS['CUR'], PHP_URL_PATH); return $BASE . $p . $u; }
    return $BASE . '/' . ltrim($u, '/');
}

/** Parse the main form: all inputs/selects + action + primary op button. */
function parse_form(string $html): ?array {
    if (!preg_match('/<form\b[^>]*>/i', $html, $fm, PREG_OFFSET_CAPTURE)) return null;
    $start = $fm[0][1];
    $end = strpos($html, '</form>', $start);
    if ($end === false) return null;
    $frag = substr($html, $start, $end - $start);
    preg_match('/<form\b[^>]*action="([^"]*)"/i', $frag, $am);
    $action = abs_url($am[1] ?? '');
    $fields = [];
    if (preg_match_all('/<input\b[^>]*>/i', $frag, $ins)) {
        foreach ($ins[0] as $tag) {
            preg_match('/name="([^"]*)"/i', $tag, $nm); if (empty($nm)) continue;
            $name = html_entity_decode($nm[1]);
            preg_match('/value="([^"]*)"/i', $tag, $vm); $val = html_entity_decode($vm[1] ?? '');
            if (preg_match('/type="(checkbox|radio)"/i', $tag, $tm)) {
                if (!preg_match('/checked/i', $tag)) continue;   // unchecked: omit
                $fields[$name] = $val !== '' ? $val : '1';
            } else {
                $fields[$name] = $val;
            }
        }
    }
    if (preg_match_all('/<select\b[^>]*name="([^"]*)"[^>]*>(.*?)<\/select>/is', $frag, $sels, PREG_SET_ORDER)) {
        foreach ($sels as $s) {
            $name = html_entity_decode($s[1]); $inner = $s[2];
            if (preg_match('/<option\b[^>]*selected[^>]*value="([^"]*)"/i', $inner, $om)) {
                $fields[$name] = html_entity_decode($om[1]);
            } elseif (preg_match('/<option\b[^>]*selected[^>]*>([^<]*)</i', $inner, $om)) {
                $fields[$name] = html_entity_decode(trim($om[1]));
            } elseif (preg_match('/<option\b[^>]*value="([^"]*)"/i', $inner, $om)) {
                $fields[$name] = html_entity_decode($om[1]);
            }
        }
    }
    $op = null;
    if (preg_match_all('/<input\b[^>]*type="submit"[^>]*>/i', $frag, $subs)) {
        foreach ($subs[0] as $tag) {
            preg_match('/value="([^"]*)"/i', $tag, $vm); preg_match('/name="([^"]*)"/i', $tag, $nm);
            $v = html_entity_decode($vm[1] ?? '');
            if (str_contains($v, 'Save and continue') || str_contains($v, 'Log in')) { $op = [$nm[1] ?? 'op', $v]; break; }
        }
        if ($op === null && isset($subs[0][0])) {
            preg_match('/value="([^"]*)"/i', $subs[0][0], $vm); preg_match('/name="([^"]*)"/i', $subs[0][0], $nm);
            $op = [$nm[1] ?? 'op', html_entity_decode($vm[1] ?? '')];
        }
    }
    return ['action' => $action, 'fields' => $fields, 'op' => $op];
}

function apply_overrides(array &$fields, array $rules): void {
    foreach ($fields as $name => $v) {
        foreach ($rules as $pat => $val) {
            if (str_contains($name, $pat)) { $fields[$name] = $val; }
        }
    }
}

function page_errors(string $html): string {
    if (!preg_match('/messages--error[\s\S]{0,5000}?<\/div>\s*<\/div>/i', $html, $mm)) return '';
    preg_match_all('/<li[^>]*>([\s\S]*?)<\/li>/i', $mm[0], $lis);
    $txt = trim(preg_replace('/\s+/', ' ', strip_tags(implode(' | ', $lis[1] ?? []))));
    return substr($txt, 0, 400);
}

/** Serialize fields the way a browser+PHP would nest them: a[b][c] -> nested arrays. */
function nested_body(array $fields): string {
    $out = [];
    foreach ($fields as $name => $val) {
        if (preg_match('/^([^\[]*)((?:\[[^\]]*\])*)$/', $name, $mm) && $mm[2] !== '') {
            preg_match_all('/\[([^\]]*)\]/', $mm[2], $bs);
            $node = &$out;
            $top = $mm[1];
            $segs = $bs[1];
            $path = [$top, ...$segs];
            foreach ($path as $i => $seg) {
                if ($i === count($path) - 1) { $node[$seg] = $val; }
                else { $node[$seg] ??= []; $node = &$node[$seg]; }
            }
            unset($node);
        } else {
            $out[$name] = $val;
        }
    }
    return http_build_query($out);
}

// ---- 1. walk the installer ----
$GLOBALS['BASE'] = $BASE;
$GLOBALS['CUR'] = "$BASE/core/install.php";
$url = "$BASE/core/install.php";
$pending = null;            // html already fetched (POST response), skip re-GET
$steps = 0; $installed = false;
$dbFormSeen = 0; $cfgFormSeen = 0; $lastFormKey = '';
while ($steps++ < 120) {
    if ($pending === null) {
        [$html, $code] = get_page($url);
        $GLOBALS['CUR'] = $url;
        echo "[step $steps] GET $code $url\n";
        if ($code >= 300 && $code < 400) {
            $loc = redirect_url();
            if ($loc === '') fail("redirect without Location at $url");
            $url = abs_url($loc); continue;
        }
        if ($code !== 200) fail("installer page HTTP $code at $url");
    } else {
        $html = $pending; $pending = null;
    }
    if (preg_match('/<meta[^>]*http-equiv="Refresh"[^>]*content="[^"]*url=([^"]*)"/i', $html, $mm)) {
        $url = abs_url($mm[1]); continue;
    }
    if (str_contains($html, 'Congratulations, you installed Drupal')) { ok('installer reports completion (step ' . $steps . ')'); $installed = true; break; }
    $form = parse_form($html);
    if ($form === null) {
        file_put_contents('/tmp/dp_noform.html', "URL: " . $GLOBALS['CUR'] . "\n" . $html);
        // batch pages: follow any explicit op= URL (do_nojs/start) or a continue link
        if (preg_match('/(https?:\/\/[^"\'\s<>]*install\.php[^"\'\s<>]*op=(?:do_nojs|start)[^"\'\s<>]*)/i', $html, $lm)
            || preg_match('/href="([^"]*install\.php[^"]*op=[^"]*)"/i', $html, $lm)) {
            $url = abs_url($lm[1]); continue;
        }
        fail("no form and no continuation at " . $GLOBALS['CUR'] . " (len " . strlen($html) . "); page dumped to /tmp/dp_noform.html");
    }
    $fields = $form['fields'];
    $fid = $fields['form_id'] ?? '';
    echo "[step $steps] form_id=$fid action={$form['action']}\n";
    if ($fid === 'install_select_language_form') { $fields['langcode'] = 'en'; }
    if ($fid === 'install_select_profile_form' && isset($fields['profile'])) { $fields['profile'] = 'standard'; }
    if ($fid === 'install_settings_form' && $dbFormSeen === 0) {
        echo '[step ' . $steps . "] settings fields: " . implode(',', array_keys($fields)) . "\n";
    }
    $isDbForm = ($fid === 'install_settings_form') || isset($fields['mysql[database]']) || array_key_exists('mysql[driver]', $fields);
    $isConfigForm = ($fid === 'install_configure_form') || isset($fields['account[name]']);
    if ($isDbForm) {
        if (++$dbFormSeen > 3) fail('database form repeats (validation loop): ' . page_errors($html));
        apply_overrides($fields, [
            'mysql[driver]' => 'mysql',
            'mysql[database]' => DB_NAME,
            'mysql[username]' => DB_USER,
            'mysql[password]' => DB_PASS,
            '[host]' => '127.0.0.1',
            '[port]' => (string)$DBPORT,
            'prefix' => 'dp_',
        ]);
        ok('database settings form #' . $dbFormSeen . ': mysql -> 127.0.0.1:' . $DBPORT . '/' . DB_NAME . ' (prefix dp_)');
    }
    if ($isConfigForm) {
        if (++$cfgFormSeen > 3) fail('site config form repeats (validation loop): ' . page_errors($html));
        apply_overrides($fields, [
            'site_name' => SITE_NAME,
            'site_mail' => ADMIN_MAIL,
            'account[name]' => ADMIN_NAME,
            'account[mail]' => ADMIN_MAIL,
            'account[pass][pass1]' => ADMIN_PASS,
            'account[pass][pass2]' => ADMIN_PASS,
        ]);
        ok('site configuration form #' . $cfgFormSeen . ': site + admin account set');
    }
    if ($form['op'] === null) fail('no submit button at ' . $GLOBALS['CUR']);
    [$opName, $opVal] = $form['op'];
    $fields[$opName] = $opVal;
    [$html2, $code2] = http($form['action'], nested_body($fields));
    $GLOBALS['CUR'] = $form['action'];
    if ($code2 >= 300 && $code2 < 400) {
        $loc = redirect_url();
        if ($loc === '') fail("POST redirect without Location at {$form['action']}");
        $url = abs_url($loc);
        // after the site-config POST Drupal redirects straight to the site (D11 has
        // no 'Congratulations' page) — leaving install.php means the install is done
        if ($cfgFormSeen > 0 && !str_contains($url, 'install.php')) {
            ok("site config accepted; redirected to $url — install complete");
            $installed = true; break;
        }
        continue;
    }
    if ($code2 !== 200) fail("form POST HTTP $code2 at {$form['action']}");
    $errs = page_errors($html2);
    if ($errs !== '') echo "[step $steps] POST errors: $errs\n";
    $pending = $html2;
}

// ---- 2. homepage renders ----
if (!$installed) fail('walker exhausted 120 steps at ' . $GLOBALS['CUR']);
$hops = 0; $cur = "$BASE/";
while (true) {
    [$home, $hcode] = get_page($cur);
    if ($hcode >= 300 && $hcode < 400) {
        $loc = redirect_url();
        if ($loc === '' || ++$hops > 5) fail("homepage redirect loop at $cur");
        ok("homepage $cur -> 302 -> $loc (following)");
        $cur = abs_url($loc); continue;
    }
    break;
}
if ($hcode !== 200) fail("homepage HTTP $hcode at $cur: " . substr(strip_tags($home), 0, 300));
if (!str_contains($home, SITE_NAME)) fail('homepage: site name not found');
ok('homepage renders (HTTP 200) with site name "' . SITE_NAME . '"');

// ---- 3. admin session through the real login form ----
// Drupal 11 authenticates the admin at the end of the install itself, so
// /user/login may answer 302 -> /user/1 right away; only fall back to a
// real form login when the session is not established yet.
$alreadyIn = false;
[$lp, $lcode] = get_page("$BASE/user/login");
if ($lcode >= 300 && $lcode < 400) {
    $dest0 = redirect_url() !== '' ? abs_url(redirect_url()) : "$BASE/user/1";
    [$dash0, $dc0] = get_page($dest0);
    if ($dc0 === 200 && str_contains($dash0, 'Log out')) {
        $alreadyIn = true;
        ok("installer authenticated the admin session: $dest0 renders with 'Log out'");
    } else {
        fail("user/login redirected to $dest0 but no admin session (HTTP $dc0)");
    }
} elseif ($lcode !== 200) {
    fail("user/login HTTP $lcode");
}
if (!$alreadyIn) {
    $lform = parse_form($lp);
    if ($lform === null) fail('login form not found');
    $lf = $lform['fields'];
    $lf['name'] = ADMIN_NAME;   // bare field names: pattern contains() would miss them
    $lf['pass'] = ADMIN_PASS;
    if ($lform['op'] !== null) { [$on, $ov] = $lform['op']; $lf[$on] = $ov; }
    [$lresp, $lcode2] = http($lform['action'], nested_body($lf));
    echo "[login] POST {$lform['action']} -> HTTP $lcode2 (fields: " . implode(',', array_keys($lf)) . ")\n";
    file_put_contents('/tmp/dp_login_resp.html', $lresp);
    if ($lcode2 !== 200 && $lcode2 !== 302 && $lcode2 !== 303) fail("login POST HTTP $lcode2");
    // after login Drupal redirects to /user/1 (clean URL) — via index.php fallback
    $dest = redirect_url() !== '' ? abs_url(redirect_url()) : "$BASE/user/1";
    [$dash, $dcode] = get_page($dest);
    if ($dcode !== 200 || !str_contains($dash, 'Log out')) {
        fail("admin session not established (HTTP $dcode after login, dest $dest): " . page_errors($lresp) . ' | ' . substr(strip_tags($dash), 0, 200));
    }
    ok("admin login OK: $dest renders with 'Log out' (bcrypt verify via wire)");
}

// ---- 4. DB-level checks through the wire (mysqli) ----
$m = @new mysqli('127.0.0.1', DB_USER, DB_PASS, DB_NAME, $DBPORT);
if ($m->connect_errno) fail("mysqli connect: {$m->connect_error}");
$m->set_charset('utf8mb4');
$rt = $m->query('SHOW TABLES'); $tables = 0; $present = [];
while ($row = $rt->fetch_row()) { $tables++; $present[$row[0]] = true; }
if (!isset($present['dp_users_field_data'])) fail("dp_users_field_data missing (tables=$tables)");
$u = $m->query("SELECT uid, name, mail, status, created, default_langcode FROM dp_users_field_data WHERE uid = 1")->fetch_assoc();
if (!$u || $u['name'] !== ADMIN_NAME) fail('uid 1 admin row wrong: ' . json_encode($u));
if ($u['created'] === null || $u['default_langcode'] != 1) {
    fail('uid 1 entity columns lost through the wire (created/default_langcode NULL): ' . json_encode($u));
}
$cfg = isset($present['dp_config']) ? $m->query("SELECT COUNT(*) AS c FROM dp_config")->fetch_assoc()['c'] : 0;
$mt  = isset($present['dp_menu_tree']) ? $m->query("SELECT COUNT(*) AS c FROM dp_menu_tree")->fetch_assoc()['c'] : 0;
$kv  = isset($present['dp_key_value']) ? $m->query("SELECT COUNT(*) AS c FROM dp_key_value")->fetch_assoc()['c'] : 0;
$wd  = isset($present['dp_watchdog']) ? $m->query("SELECT COUNT(*) AS c FROM dp_watchdog")->fetch_assoc()['c'] : 0;
printf("DB: tables=%s uid1=%s(config rows=%s menu_tree=%s key_value=%s watchdog=%s)\n", $tables, $u['name'], $cfg, $mt, $kv, $wd);
if ((int)$cfg < 100) fail("config rows too few: $cfg");
// prepared-statement roundtrip through the wire (admin read path)
$st = $m->prepare('SELECT name, mail FROM dp_users_field_data WHERE uid = ?');
if (!$st) fail('prepare SELECT: ' . $m->error);
$uid = 1; $st->bind_param('i', $uid); $st->execute();
$row = $st->get_result()->fetch_assoc(); $st->close();
if (!$row || $row['mail'] !== ADMIN_MAIL) fail('prepared SELECT roundtrip failed: ' . json_encode($row));
ok('prepared SELECT roundtrip via wire OK (uid=1)');
$m->close();

echo "DRUPAL_E2E_OK\n";
@unlink($JAR);
