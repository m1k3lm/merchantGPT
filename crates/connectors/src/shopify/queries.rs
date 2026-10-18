//! GraphQL documents for the Admin API. Page sizes are capped by the callers
//! so a query stays under Shopify's 1,000-point single-query cost limit.

pub const SHOP: &str = "{ shop { name currencyCode } }";

const VARIANT: &str = "fragment V on ProductVariant {
  id title sku price compareAtPrice inventoryQuantity
  inventoryItem { id tracked }
  product { id title status }
}";

pub fn variants() -> String {
    format!(
        "query Variants($first: Int!, $after: String, $query: String) {{
  productVariants(first: $first, after: $after, query: $query, sortKey: ID) {{
    nodes {{ ...V }}
    pageInfo {{ hasNextPage endCursor }}
  }}
}}
{VARIANT}"
    )
}

pub fn variant() -> String {
    format!(
        "query Variant($id: ID!) {{
  productVariant(id: $id) {{
    ...V
    inventoryItem {{
      inventoryLevels(first: 2) {{ nodes {{ location {{ id }} }} }}
    }}
  }}
}}
{VARIANT}"
    )
}

pub const VARIANTS_COUNT: &str = "{ productVariantsCount(limit: null) { count } }";

/// Walks to a later page without fetching its items.
pub fn cursor(connection: &str) -> String {
    format!(
        "query Cursor($first: Int!, $after: String, $query: String) {{
  {connection}(first: $first, after: $after, query: $query) {{
    pageInfo {{ hasNextPage endCursor }}
  }}
}}"
    )
}

const ORDER: &str = "fragment O on Order {
  id name createdAt cancelledAt closed
  displayFinancialStatus displayFulfillmentStatus
  totalPriceSet { shopMoney { amount currencyCode } }
  email
  customer { id firstName lastName }
  billingAddress { firstName lastName }
}";

const LINE_ITEMS: &str = "lineItems(first: $lines) {
  nodes { name currentQuantity discountedTotalSet { shopMoney { amount } } variant { id } }
}";

pub fn orders() -> String {
    format!(
        "query Orders($first: Int!, $after: String, $query: String, $lines: Int!) {{
  orders(first: $first, after: $after, query: $query, sortKey: CREATED_AT, reverse: true) {{
    nodes {{ ...O {LINE_ITEMS} }}
    pageInfo {{ hasNextPage endCursor }}
  }}
}}
{ORDER}"
    )
}

pub fn order() -> String {
    format!(
        "query Order($id: ID!, $lines: Int!) {{
  order(id: $id) {{ ...O {LINE_ITEMS} }}
}}
{ORDER}"
    )
}

pub const ORDERS_COUNT: &str =
    "query OrdersCount($query: String) { ordersCount(query: $query, limit: null) { count } }";

pub const REPORT: &str = "query Report($after: String, $query: String) {
  orders(first: 100, after: $after, query: $query, sortKey: CREATED_AT) {
    nodes {
      cancelledAt
      currentSubtotalLineItemsQuantity
      totalPriceSet { shopMoney { amount currencyCode } }
      currentSubtotalPriceSet { shopMoney { amount } }
    }
    pageInfo { hasNextPage endCursor }
  }
}";

pub const CUSTOMERS: &str = "query Customers($first: Int!, $after: String, $query: String) {
  customers(first: $first, after: $after, query: $query) {
    nodes {
      id firstName lastName numberOfOrders
      defaultEmailAddress { emailAddress }
      amountSpent { amount }
    }
    pageInfo { hasNextPage endCursor }
  }
}";

pub const CUSTOMERS_COUNT: &str = "{ customersCount(limit: null) { count } }";

pub const PRODUCT_UPDATE: &str = "mutation ProductUpdate($product: ProductUpdateInput!) {
  productUpdate(product: $product) { userErrors { field message } }
}";

pub const VARIANTS_UPDATE: &str =
    "mutation VariantsUpdate($productId: ID!, $variants: [ProductVariantsBulkInput!]!) {
  productVariantsBulkUpdate(productId: $productId, variants: $variants) { userErrors { field message } }
}";

pub const SET_STOCK: &str =
    "mutation SetStock($input: InventorySetQuantitiesInput!, $idempotencyKey: String!) {
  inventorySetQuantities(input: $input) @idempotent(key: $idempotencyKey) {
    userErrors { field message }
  }
}";

pub const ORDER_CANCEL: &str =
    "mutation Cancel($orderId: ID!, $reason: OrderCancelReason!, $restock: Boolean!, $staffNote: String) {
  orderCancel(orderId: $orderId, reason: $reason, restock: $restock, notifyCustomer: false, staffNote: $staffNote) {
    orderCancelUserErrors { field message }
  }
}";

pub const ORDER_CLOSE: &str = "mutation Close($input: OrderCloseInput!) {
  orderClose(input: $input) { userErrors { field message } }
}";

pub const ORDER_OPEN: &str = "mutation Open($input: OrderOpenInput!) {
  orderOpen(input: $input) { userErrors { field message } }
}";

pub const ORDER_MARK_PAID: &str = "mutation MarkPaid($input: OrderMarkAsPaidInput!) {
  orderMarkAsPaid(input: $input) { userErrors { field message } }
}";
