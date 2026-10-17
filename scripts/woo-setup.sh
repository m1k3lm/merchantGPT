#!/usr/bin/env bash
# Installs WordPress + WooCommerce with sample data and prints REST API keys.
# Run inside the woo-cli container (see docker-compose.yml). Idempotent.
set -euo pipefail
cd /var/www/html

for _ in $(seq 1 60); do
  wp db check >/dev/null 2>&1 && break
  sleep 2
done

if ! wp core is-installed 2>/dev/null; then
  wp core install --url=http://localhost:8080 --title="merchantGPT test shop" \
    --admin_user=admin --admin_password=admin --admin_email=admin@example.com --skip-email
fi
wp rewrite structure '/%postname%/' --hard >/dev/null
wp plugin is-installed woocommerce || wp plugin install woocommerce
wp plugin is-active woocommerce || wp plugin activate woocommerce
wp option update woocommerce_currency EUR >/dev/null

if [ "$(wp wc product list --user=1 --format=count)" = "0" ]; then
  hoodie=$(wp wc product create --user=1 --name="Blue Hoodie" --sku=HOOD-BLU --regular_price=42.00 \
    --manage_stock=true --stock_quantity=7 --porcelain)
  wp wc product create --user=1 --name="Red Cap" --sku=CAP-RED --regular_price=15.00 --porcelain >/dev/null
  customer=$(wp wc customer create --user=1 --email=jo@example.com --first_name=Jo --last_name=Smith --porcelain)
  wp wc shop_order create --user=1 --status=processing --customer_id="$customer" \
    --billing='{"first_name":"Jo","last_name":"Smith","email":"jo@example.com"}' \
    --line_items="[{\"product_id\":$hoodie,\"quantity\":2}]" --porcelain >/dev/null
fi

wp eval '
global $wpdb;
$ck = "ck_" . wc_rand_hash();
$cs = "cs_" . wc_rand_hash();
$wpdb->insert($wpdb->prefix . "woocommerce_api_keys", array(
  "user_id" => 1, "description" => "merchantGPT", "permissions" => "read_write",
  "consumer_key" => wc_api_hash($ck), "consumer_secret" => $cs, "truncated_key" => substr($ck, -7),
));
echo "WOO_URL=http://localhost:8080\nWOO_CK=$ck\nWOO_CS=$cs\n";
'
