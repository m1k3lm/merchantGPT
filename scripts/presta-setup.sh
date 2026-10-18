#!/usr/bin/env bash
# Enables the webservice on the `presta` compose store (PrestaShop with its
# demo catalog and orders), creates a key for merchantGPT's resources and
# prints PRESTA_URL / PRESTA_KEY. Run inside the presta container. Idempotent:
# the key is fixed, since this store only ever runs locally.
set -euo pipefail
cd /var/www/html

for _ in $(seq 1 120); do
  [ -f app/config/parameters.php ] && [ ! -d install ] && break
  sleep 5
done

php <<'PHP'
<?php
require 'config/config.inc.php';
$key = 'MERCHANTGPTLOCALPRESTATESTKEY001';
Configuration::updateValue('PS_WEBSERVICE', 1);
Configuration::updateValue('PS_REWRITING_SETTINGS', 1);
Tools::generateHtaccess();
if (!WebserviceKey::keyExists($key)) {
    $account = new WebserviceKey();
    $account->key = $key;
    $account->description = 'merchantGPT local tests';
    $account->active = 1;
    $account->add();
}
// Re-applied on every run, so an existing key gains newly used resources.
$id = (int) Db::getInstance()->getValue(
    'SELECT id_webservice_account FROM ' . _DB_PREFIX_ . "webservice_account WHERE `key` = '" . pSQL($key) . "'"
);
$methods = array_fill_keys(['GET', 'PUT', 'POST', 'PATCH', 'DELETE', 'HEAD'], 'on');
$resources = ['products', 'categories', 'combinations', 'stock_availables', 'specific_prices',
    'orders', 'order_states', 'order_histories', 'order_carriers', 'carriers', 'customers',
    'addresses', 'countries', 'states', 'currencies', 'languages', 'carts', 'cart_rules',
    'customer_threads', 'customer_messages'];
WebserviceKey::setPermissionForAccount($id, array_fill_keys($resources, $methods));
echo "PRESTA_URL=http://localhost:8081\nPRESTA_KEY=$key\n";
PHP
