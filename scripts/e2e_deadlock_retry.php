<?php
// E2E: deadlock (errno 1213) + Woo-style retry over the MySQL wire adapter.
// Two concurrent "live order" transactions lock the same inventory rows in
// OPPOSITE order (classic checkout deadlock); whoever the engine picks as
// the deadlock victim retries the WHOLE transaction — the pattern
// WooCommerce applies on SQLSTATE 40001 / errno 1213. Both retry wrappers
// are exercised: phase 1 makes the worker the victim, phase 2 — main.
//
//   phase 1: main holds SKU-A then orders A,B; worker holds SKU-B, sleeps,
//            then orders B,A  → worker's second request closes the cycle
//   phase 2: same holds, worker orders B,A immediately → main's second
//            request closes the cycle
//
// Usage: php e2e_deadlock_retry.php <main|worker> <phase>
// The main process orchestrates (spawns the worker via PHP_BINARY),
// runs both phases and prints one JSON verdict line per phase.
//
// Artifacts verified per phase: orders committed = 2, final stock = 9/9,
// the intended session saw >= 1 deadlock, zero failed orders.

error_reporting(E_ALL & ~E_DEPRECATED);

const HOST = '127.0.0.1';
const PORT = 13307;
const USER = 'wp';
const PASS = 'wppass';
const DB   = 'wordpress';
const MAX_RETRIES = 10;
const MAIN_SLEEP_US = 700000;   // main orders its second row 0.7 s in
const WORKER_LATE_SLEEP_US = 1200000; // phase 1: worker waits 1.2 s → worker victim

function connect(): mysqli {
    $m = mysqli_init();
    if (!$m) { fwrite(STDERR, "mysqli_init failed\n"); exit(1); }
    if (!@$m->real_connect(HOST, USER, PASS, DB, PORT)) {
        fwrite(STDERR, 'connect failed: ' . mysqli_connect_error() . "\n");
        exit(1);
    }
    $m->set_charset('utf8mb4');
    // generous lock budget: deadlock detection must fire long before this
    $m->query('SET innodb_lock_wait_timeout = 5');
    return $m;
}

function q(mysqli $m, string $sql) {
    $r = $m->query($sql);
    if ($r === false) {
        throw new RuntimeException("[$sql] " . $m->error, $m->errno);
    }
    return $r;
}

/**
 * Woo-style transaction wrapper: retry the whole tx on deadlock (1213) and
 * lock-wait timeout (1205). Anything else fails the order immediately.
 */
function tx_with_retry(mysqli $m, callable $body, string $tag, array &$stats): void {
    for ($attempt = 1; $attempt <= MAX_RETRIES; $attempt++) {
        try {
            $m->begin_transaction();
            $body($m);
            if (!$m->commit()) {
                throw new RuntimeException('commit failed: ' . $m->error, $m->errno);
            }
            $stats['retries'] = max($stats['retries'], $attempt - 1);
            return;
        } catch (Throwable $e) {
            $errno = (int)$e->getCode();
            @$m->rollback();
            if ($errno === 1213) {         // SQLSTATE 40001 — deadlock victim
                $stats['deadlocks']++;
            } elseif ($errno === 1205) {   // lock wait timeout
                $stats['lock_timeouts']++;
            } elseif ($errno === 3572) {   // NOWAIT refused
                $stats['nowait']++;
            } else {
                $stats['failures']++;
                fwrite(STDERR, "[$tag] unexpected errno $errno: {$e->getMessage()}\n");
                throw $e;
            }
            if ($attempt >= MAX_RETRIES) {
                $stats['failures']++;
                throw new RuntimeException("[$tag] retry budget exhausted", $errno);
            }
            usleep(random_int(20000, 90000)); // small randomized backoff
        }
    }
}

/** SELECT … FOR UPDATE inside the open tx — a pure row hold, no DML.
 *  All holds come FIRST in the tx: the gate is single-writer (a failed
 *  statement does not un-apply its earlier DML), so the deadlock must hit
 *  while the tx is write-free — then the Woo-style retry re-runs it with
 *  no duplicated effects. */
function hold_row(mysqli $c, string $sku): void {
    $st = $c->prepare('SELECT qty FROM inventory WHERE sku = ? FOR UPDATE');
    $st->bind_param('s', $sku);
    $st->execute();
    $st->bind_result($stock);
    if (!$st->fetch()) {
        $st->close();
        throw new RuntimeException("sku $sku vanished (hold)", 0);
    }
    $st->close();
    if ($stock < 1) {
        throw new RuntimeException("insufficient stock for $sku", 0);
    }
}

/** INSERT the order row + reduce stock — runs AFTER both rows are held. */
function apply_order(mysqli $c, string $sku, int $qty): void {
    $ins = $c->prepare('INSERT INTO orders (sku, qty) VALUES (?, ?)');
    $ins->bind_param('si', $sku, $qty);
    $ins->execute();
    $ins->close();
    $up = $c->prepare('UPDATE inventory SET qty = qty - ? WHERE sku = ?');
    $up->bind_param('is', $qty, $sku);
    $up->execute();
    $up->close();
}

/** Waits for a coordination file, up to $timeout seconds. */
function wait_for(string $file, float $timeout): bool {
    $deadline = microtime(true) + $timeout;
    while (microtime(true) < $deadline) {
        if (file_exists($file)) { return true; }
        usleep(20000);
    }
    return false;
}

function signal(string $file): void { @file_put_contents($file, '1'); }

// ---------------------------------------------------------------------------
$mode  = $argv[1] ?? 'main';
$phase = (int)($argv[2] ?? 1);
$stats = ['deadlocks' => 0, 'lock_timeouts' => 0, 'nowait' => 0, 'retries' => 0, 'failures' => 0];

if ($mode === 'main') {
    // ---- phase 1 ----
    $r1 = drl_phase(1);
    // ---- phase 2 (roles swapped: main becomes the victim) ----
    $r2 = drl_phase(2);
    $ok = $r1['verdict'] === 'PASS' && $r2['verdict'] === 'PASS';
    echo json_encode([
        'phases'  => [$r1, $r2],
        'verdict' => $ok ? 'PASS' : 'FAIL',
    ], JSON_UNESCAPED_UNICODE), "\n";
    exit($ok ? 0 : 2);
}

// ----------------------------- worker mode ---------------------------------
$work = sys_get_temp_dir() . "/drl_p$phase";
try {
    $m = connect();
    tx_with_retry($m, function (mysqli $c) use ($phase, $work): void {
        // hold SKU-B for the whole transaction (locks survive the signals)
        hold_row($c, 'SKU-B');
        signal("$work.worker_locked");
        if (!wait_for("$work.main_locked", 15)) {
            throw new RuntimeException('main never locked its row', 0);
        }
        // phase 1: let main block on our hold first → main's request opens
        // the waits-for edge, ours closes it → WE are the victim.
        if ($phase === 1) {
            usleep(WORKER_LATE_SLEEP_US);
        }
        // phase 2: rush in immediately → main's later request closes the
        // cycle → MAIN is the victim.
        hold_row($c, 'SKU-A'); // blocks on main's hold; victim on cycle
        apply_order($c, 'SKU-B', 1);
        apply_order($c, 'SKU-A', 1);
    }, 'worker', $stats);
    $m->close();
    file_put_contents("$work.done", json_encode($stats));
} catch (Throwable $e) {
    file_put_contents("$work.done", json_encode($stats));
    fwrite(STDERR, '[worker] fatal: ' . $e->getMessage() . "\n");
    exit(1);
}

// ------------------------------ orchestrator -------------------------------
function drl_phase(int $phase): array {
    $work = sys_get_temp_dir() . "/drl_p$phase";
    foreach (glob("$work.*") ?: [] as $f) { @unlink($f); }

    $m = connect();
    q($m, 'DROP TABLE IF EXISTS orders');
    q($m, 'DROP TABLE IF EXISTS inventory');
    q($m, 'CREATE TABLE inventory (sku varchar(20) NOT NULL PRIMARY KEY, qty int NOT NULL, updated datetime NULL)');
    q($m, "INSERT INTO inventory (sku, qty) VALUES ('SKU-A', 10), ('SKU-B', 10)");
    q($m, 'CREATE TABLE orders (id bigint unsigned NOT NULL AUTO_INCREMENT PRIMARY KEY, sku varchar(20) NOT NULL, qty int NOT NULL, created datetime NULL)');

    $cmd = PHP_BINARY . ' ' . escapeshellarg(__FILE__) . " worker $phase > "
         . sys_get_temp_dir() . "/drl_worker_p$phase.log 2>&1 &";
    shell_exec($cmd);
    if (!wait_for("$work.worker_locked", 15)) {
        return ['phase' => $phase, 'verdict' => 'FAIL', 'error' => 'worker never locked its row'];
    }

    $stats = ['deadlocks' => 0, 'lock_timeouts' => 0, 'nowait' => 0, 'retries' => 0, 'failures' => 0];
    try {
        tx_with_retry($m, function (mysqli $c) use ($work): void {
            // hold SKU-A for the whole transaction
            hold_row($c, 'SKU-A');
            signal("$work.main_locked");
            usleep(MAIN_SLEEP_US); // let the worker settle on its hold
            hold_row($c, 'SKU-B'); // blocks on worker's hold; victim on cycle
            apply_order($c, 'SKU-A', 1);
            apply_order($c, 'SKU-B', 1);
        }, 'main', $stats);
        $m->close();
    } catch (Throwable $e) {
        return ['phase' => $phase, 'verdict' => 'FAIL', 'error' => $e->getMessage(), 'stats' => $stats];
    }

    if (!wait_for("$work.done", 60)) {
        return ['phase' => $phase, 'verdict' => 'FAIL', 'error' => 'worker never finished'];
    }
    $worker = json_decode(file_get_contents("$work.done"), true) ?: ['failures' => 99];

    // artifacts on a fresh connection
    $m = connect();
    $orders = (int)q($m, 'SELECT COUNT(*) AS c FROM orders')->fetch_assoc()['c'];
    $a = (int)q($m, "SELECT qty FROM inventory WHERE sku = 'SKU-A'")->fetch_assoc()['qty'];
    $b = (int)q($m, "SELECT qty FROM inventory WHERE sku = 'SKU-B'")->fetch_assoc()['qty'];
    $order_rows = q($m, 'SELECT sku, qty FROM orders ORDER BY id')->fetch_all(MYSQLI_ASSOC);
    $m->close();

    // Each session places exactly one order per sku → 4 orders, stock 8/8.
    // Phase 1 victim = worker, phase 2 victim = main.
    $victim_deadlocks = $phase === 1 ? ($worker['deadlocks'] ?? 0) : $stats['deadlocks'];
    $total_failures   = $stats['failures'] + ($worker['failures'] ?? 0);
    $verdict = ($orders === 4 && $a === 8 && $b === 8 && $total_failures === 0 && $victim_deadlocks >= 1)
        ? 'PASS' : 'FAIL';

    return [
        'phase'              => $phase,
        'expected_victim'    => $phase === 1 ? 'worker' : 'main',
        'victim_deadlocks'   => $victim_deadlocks,
        'main_deadlocks'     => $stats['deadlocks'],
        'worker_deadlocks'   => $worker['deadlocks'] ?? null,
        'main_lock_timeouts' => $stats['lock_timeouts'],
        'worker_lock_timeouts' => $worker['lock_timeouts'] ?? null,
        'main_retries'       => $stats['retries'],
        'worker_retries'     => $worker['retries'] ?? null,
        'failures'           => $total_failures,
        'orders'             => $orders,
        'order_rows'         => $order_rows,
        'stock_a'            => $a,
        'stock_b'            => $b,
        'verdict'            => $verdict,
    ];
}
