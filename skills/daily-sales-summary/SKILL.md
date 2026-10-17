---
name: daily-sales-summary
description: Summarise yesterday's sales, orders that need attention and low-stock products.
---

Produce a short morning briefing for the merchant.

1. Call `shop_sales_report` for yesterday (from and to are both yesterday's date) and for the same weekday one week earlier. Report gross and net sales, number of orders and items, and the change versus last week as a percentage.
2. Call `shop_list_orders` with status `processing` and status `on-hold`. List the orders waiting longest first, with number, customer and total.
3. Call `shop_list_products` and point out products whose `stock_quantity` is 5 or less.
4. End with at most three suggested actions. Do not change anything in the shop; only suggest.

Keep it under 200 words and use a table for the orders.
