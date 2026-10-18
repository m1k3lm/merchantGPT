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

/// One variant with what writes need: its stock locations, how many variants
/// its product has and the product's collections.
pub fn variant() -> String {
    format!(
        "query Variant($id: ID!) {{
  productVariant(id: $id) {{
    ...V
    inventoryItem {{
      inventoryLevels(first: 2) {{ nodes {{ location {{ id }} }} }}
    }}
    product {{
      variantsCount {{ count }}
      collections(first: 50) {{ nodes {{ id }} }}
    }}
  }}
}}
{VARIANT}"
    )
}

pub const VARIANTS_COUNT: &str = "{ productVariantsCount(limit: null) { count } }";

/// Walks to a later page without fetching its items. `ordering` must match
/// the page query's, or the cursors point into a differently sorted list.
pub fn cursor(connection: &str, ordering: &str) -> String {
    format!(
        "query Cursor($first: Int!, $after: String, $query: String) {{
  {connection}(first: $first, after: $after, query: $query{ordering}) {{
    pageInfo {{ hasNextPage endCursor }}
  }}
}}"
    )
}

pub const BY_ID: &str = ", sortKey: ID";
pub const NEWEST_ORDERS: &str = ", sortKey: CREATED_AT, reverse: true";
pub const NEWEST_FIRST: &str = ", reverse: true";

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

const CUSTOMER: &str = "fragment C on Customer {
  id firstName lastName numberOfOrders
  defaultEmailAddress { emailAddress marketingState }
  defaultPhoneNumber { phoneNumber }
  amountSpent { amount }
}";

const ADDRESS: &str = "fragment A on MailingAddress {
  id firstName lastName company address1 address2 city zip provinceCode countryCodeV2 phone
}";

pub fn customers() -> String {
    format!(
        "query Customers($first: Int!, $after: String, $query: String) {{
  customers(first: $first, after: $after, query: $query) {{
    nodes {{ ...C }}
    pageInfo {{ hasNextPage endCursor }}
  }}
}}
{CUSTOMER}"
    )
}

pub fn customer() -> String {
    format!(
        "query Customer($id: ID!) {{
  customer(id: $id) {{
    ...C
    defaultAddress {{ id }}
    addressesV2(first: 50) {{ nodes {{ ...A }} }}
  }}
}}
{CUSTOMER}
{ADDRESS}"
    )
}

pub const CUSTOMER_CREATE: &str = "mutation CustomerCreate($input: CustomerInput!) {
  customerCreate(input: $input) { customer { id } userErrors { field message } }
}";

pub const CUSTOMER_UPDATE: &str = "mutation CustomerUpdate($input: CustomerInput!) {
  customerUpdate(input: $input) { customer { id } userErrors { field message } }
}";

pub const CUSTOMER_CONSENT: &str =
    "mutation Consent($input: CustomerEmailMarketingConsentUpdateInput!) {
  customerEmailMarketingConsentUpdate(input: $input) { userErrors { field message } }
}";

pub const CUSTOMER_DELETE: &str = "mutation CustomerDelete($input: CustomerDeleteInput!) {
  customerDelete(input: $input) { deletedCustomerId userErrors { field message } }
}";

pub fn address_create() -> String {
    format!(
        "mutation AddressCreate($customerId: ID!, $address: MailingAddressInput!, $setAsDefault: Boolean) {{
  customerAddressCreate(customerId: $customerId, address: $address, setAsDefault: $setAsDefault) {{
    address {{ ...A }}
    userErrors {{ field message }}
  }}
}}
{ADDRESS}"
    )
}

pub fn address_update() -> String {
    format!(
        "mutation AddressUpdate($customerId: ID!, $addressId: ID!, $address: MailingAddressInput!, $setAsDefault: Boolean) {{
  customerAddressUpdate(customerId: $customerId, addressId: $addressId, address: $address, setAsDefault: $setAsDefault) {{
    address {{ ...A }}
    userErrors {{ field message }}
  }}
}}
{ADDRESS}"
    )
}

pub const ADDRESS_DELETE: &str = "mutation AddressDelete($customerId: ID!, $addressId: ID!) {
  customerAddressDelete(customerId: $customerId, addressId: $addressId) {
    deletedAddressId
    userErrors { field message }
  }
}";

const COLLECTION: &str = "fragment K on Collection {
  id title descriptionHtml productsCount { count }
}";

pub fn collections() -> String {
    format!(
        "query Collections($first: Int!, $after: String, $query: String) {{
  collections(first: $first, after: $after, query: $query) {{
    nodes {{ ...K }}
    pageInfo {{ hasNextPage endCursor }}
  }}
}}
{COLLECTION}"
    )
}

pub fn collection_create() -> String {
    format!(
        "mutation CollectionCreate($collection: CollectionCreateInput!) {{
  collectionCreate(collection: $collection) {{ collection {{ ...K }} userErrors {{ field message }} }}
}}
{COLLECTION}"
    )
}

pub fn collection_update() -> String {
    format!(
        "mutation CollectionUpdate($collection: CollectionUpdateInput!) {{
  collectionUpdate(collection: $collection) {{ collection {{ ...K }} userErrors {{ field message }} }}
}}
{COLLECTION}"
    )
}

pub const COLLECTION_DELETE: &str = "mutation CollectionDelete($input: CollectionDeleteInput!) {
  collectionDelete(input: $input) { deletedCollectionId userErrors { field message } }
}";

pub const COLLECTION_SOURCES: &str = "query CollectionSources($id: ID!) {
  collection(id: $id) { sources { __typename ... on CollectionConditionsSource { id } } }
}";

pub const COLLECTION_SOURCES_UPDATE: &str =
    "mutation CollectionSourcesUpdate($collection: CollectionUpdateInput!) {
  collectionUpdate(collection: $collection) { userErrors { field message } }
}";

pub const PRODUCT_CREATE: &str = "mutation ProductCreate($product: ProductCreateInput!) {
  productCreate(product: $product) {
    product { id variants(first: 1) { nodes { id } } }
    userErrors { field message }
  }
}";

pub const PRODUCT_DELETE: &str = "mutation ProductDelete($input: ProductDeleteInput!) {
  productDelete(input: $input) { deletedProductId userErrors { field message } }
}";

pub const VARIANTS_DELETE: &str = "mutation VariantsDelete($productId: ID!, $variantsIds: [ID!]!) {
  productVariantsBulkDelete(productId: $productId, variantsIds: $variantsIds) {
    userErrors { field message }
  }
}";

pub const LOCATIONS: &str = "{ locations(first: 2) { nodes { id } } }";

pub const INVENTORY_ACTIVATE: &str =
    "mutation Activate($inventoryItemId: ID!, $locationId: ID!, $available: Int, $idempotencyKey: String!) {
  inventoryActivate(inventoryItemId: $inventoryItemId, locationId: $locationId, available: $available) @idempotent(key: $idempotencyKey) {
    userErrors { field message }
  }
}";

pub const ORDER_CREATE: &str =
    "mutation OrderCreate($order: OrderCreateOrderInput!, $options: OrderCreateOptionsInput) {
  orderCreate(order: $order, options: $options) { order { id } userErrors { field message } }
}";

pub const ORDER_NOTE: &str = "query OrderNote($id: ID!) { order(id: $id) { id note } }";

pub const ORDER_UPDATE: &str = "mutation OrderUpdate($input: OrderInput!) {
  orderUpdate(input: $input) { userErrors { field message } }
}";

pub const SHOP_CURRENCY: &str = "{ shop { currencyCode } }";

const DISCOUNT: &str = "fragment D on DiscountCodeBasic {
  title status startsAt endsAt usageLimit asyncUsageCount
  codes(first: 1) { nodes { code } }
  customerGets {
    value {
      __typename
      ... on DiscountPercentage { percentage }
      ... on DiscountAmount { amount { amount } appliesOnEachItem }
    }
  }
  minimumRequirement {
    __typename
    ... on DiscountMinimumSubtotal { greaterThanOrEqualToSubtotal { amount } }
  }
}";

pub fn discounts() -> String {
    format!(
        "query Discounts($first: Int!, $after: String, $query: String) {{
  discountNodes(first: $first, after: $after, query: $query) {{
    nodes {{ id discount {{ __typename ...D }} }}
    pageInfo {{ hasNextPage endCursor }}
  }}
}}
{DISCOUNT}"
    )
}

pub fn discount() -> String {
    format!(
        "query Discount($id: ID!) {{
  discountNode(id: $id) {{ id discount {{ __typename ...D }} }}
}}
{DISCOUNT}"
    )
}

pub const DISCOUNT_CREATE: &str =
    "mutation DiscountCreate($basicCodeDiscount: DiscountCodeBasicInput!) {
  discountCodeBasicCreate(basicCodeDiscount: $basicCodeDiscount) {
    codeDiscountNode { id }
    userErrors { field message }
  }
}";

pub const DISCOUNT_UPDATE: &str =
    "mutation DiscountUpdate($id: ID!, $basicCodeDiscount: DiscountCodeBasicInput!) {
  discountCodeBasicUpdate(id: $id, basicCodeDiscount: $basicCodeDiscount) {
    codeDiscountNode { id }
    userErrors { field message }
  }
}";

pub const DISCOUNT_ACTIVATE: &str = "mutation DiscountActivate($id: ID!) {
  discountCodeActivate(id: $id) { userErrors { field message } }
}";

pub const DISCOUNT_DEACTIVATE: &str = "mutation DiscountDeactivate($id: ID!) {
  discountCodeDeactivate(id: $id) { userErrors { field message } }
}";

pub const DISCOUNT_DELETE: &str = "mutation DiscountDelete($id: ID!) {
  discountCodeDelete(id: $id) { deletedCodeDiscountId userErrors { field message } }
}";

pub const ABANDONED: &str = "query Abandoned($first: Int!, $after: String, $query: String) {
  abandonedCheckouts(first: $first, after: $after, query: $query, reverse: true) {
    nodes {
      id name abandonedCheckoutUrl updatedAt
      customer { id firstName lastName defaultEmailAddress { emailAddress } }
      totalPriceSet { shopMoney { amount currencyCode } }
      lineItems(first: 10) {
        nodes {
          title variantTitle quantity
          variant { id }
          discountedTotalPriceSet { shopMoney { amount } }
        }
      }
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
