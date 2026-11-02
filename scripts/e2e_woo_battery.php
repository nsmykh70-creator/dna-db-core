<?php
// WooCommerce E2E battery: identical script on MariaDB reference and DNA-DB.
// Product -> order (HPOS/CRUD) -> line item -> totals -> stock reduction.
// Output: single JSON line with artifact ids/values.

$t0 = microtime(true);

// 1) Simple product with stock management
$p = new WC_Product_Simple();
$p->set_name('Тестовый товар DNA');
$p->set_regular_price('250.00');
$p->set_manage_stock(true);
$p->set_stock_quantity(10);
$pid = $p->save();

$t1 = microtime(true);

// 2) Order for customer #1 (admin), processing
$order = wc_create_order(array('customer_id' => 1, 'status' => 'processing'));
$order->add_product(wc_get_product($pid), 2);
$order->set_billing_first_name('Иван');
$order->set_billing_last_name('Петров');
$order->set_billing_city('Москва');
$order->set_billing_address_1('Тверская, 1');
$order->set_shipping_total('100.00');
$order->calculate_totals();
$oid = $order->get_id();
$order->save();

$t2 = microtime(true);

// 3) Reduce stock (checkout path)
wc_reduce_stock_levels($oid);

$t3 = microtime(true);

// 4) Re-read artifacts through WC CRUD
$p2 = wc_get_product($pid);
$o2 = wc_get_order($oid);

$out = array(
    'product_id'       => $pid,
    'product_type'     => $p2->get_type(),
    'price'            => $p2->get_price(),
    'stock_after'      => $p2->get_stock_quantity(),
    'order_id'         => $oid,
    'order_status'     => $o2->get_status(),
    'order_total'      => $o2->get_total(),
    'order_currency'   => $o2->get_currency(),
    'billing_city'     => $o2->get_billing_city(),
    'billing_name'     => $o2->get_billing_first_name(),
    'item_count'       => $o2->get_item_count(),
    'items'            => array_values(array_map(function ($it) {
        return array('name' => $it->get_name(), 'qty' => $it->get_quantity(), 'total' => $it->get_total());
    }, array_values($o2->get_items()))),
    't_product_ms'     => round(($t1 - $t0) * 1000),
    't_order_ms'       => round(($t2 - $t1) * 1000),
    't_stock_ms'       => round(($t3 - $t2) * 1000),
);
echo json_encode($out, JSON_UNESCAPED_UNICODE), "\n";
