//! Product creation and categories.

use serde_json::{json, Map, Value};

use super::criteria::Filter;
use super::{bad_request, entity_id, types, Magento};
use crate::domain::*;
use crate::transport::Method;
use crate::ConnectorError;

/// The "Default" product attribute set of a standard install.
const DEFAULT_ATTRIBUTE_SET: u64 = 4;
/// Visible in catalog and search.
const VISIBLE_EVERYWHERE: u64 = 4;

impl Magento {
    /// Creates a simple product in every website. Magento addresses products
    /// by SKU, so one is required.
    pub(super) async fn create_product_impl(
        &self,
        p: &ProductPatch,
    ) -> Result<Product, ConnectorError> {
        let sku = p
            .sku
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| bad_request("Magento needs a SKU to create a product"))?;
        let mut product = Self::product_body(sku, p)?;
        product.insert("attribute_set_id".into(), json!(DEFAULT_ATTRIBUTE_SET));
        product.insert("type_id".into(), json!("simple"));
        product.insert("visibility".into(), json!(VISIBLE_EVERYWHERE));
        product.entry("status").or_insert(json!(1));
        let websites = self.website_ids().await?;
        let ext = product
            .entry("extension_attributes")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(ext) = ext {
            ext.insert("website_ids".into(), json!(websites));
        }
        let v = self
            .write(
                Method::Post,
                "all/",
                "/products",
                Some(json!({"product": product})),
            )
            .await?;
        types::product(&v)
    }

    pub(super) async fn list_categories_impl(
        &self,
        q: &CategoryQuery,
    ) -> Result<Page<Category>, ConnectorError> {
        let mut c = Self::criteria(q.page, q.per_page).sort("path", "ASC");
        if let Some(s) = q.search.as_deref().filter(|s| !s.trim().is_empty()) {
            c = c.and(vec![Filter::like("name", s)]);
        }
        self.list("/categories/list", c, types::category).await
    }

    async fn category(&self, id: &str) -> Result<Category, ConnectorError> {
        types::category(&self.get(&format!("/categories/{id}"), &[]).await?)
    }

    /// New categories go under the default store's root category unless a
    /// parent is given.
    pub(super) async fn create_category_impl(
        &self,
        c: &CategoryPatch,
    ) -> Result<Category, ConnectorError> {
        let name = c
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .ok_or_else(|| bad_request("a category needs a name"))?;
        let parent = match c.parent_id.as_deref() {
            Some(p) => entity_id("category", p)?.to_string(),
            None => self.default_root_category().await?,
        };
        let mut category = Map::new();
        category.insert("name".into(), json!(name));
        category.insert(
            "parent_id".into(),
            json!(parent.parse::<u64>().unwrap_or(0)),
        );
        category.insert("is_active".into(), json!(true));
        category.insert("include_in_menu".into(), json!(true));
        if let Some(d) = &c.description {
            category.insert(
                "custom_attributes".into(),
                json!([{"attribute_code": "description", "value": d}]),
            );
        }
        let v = self
            .write(
                Method::Post,
                "all/",
                "/categories",
                Some(json!({"category": category})),
            )
            .await?;
        types::category(&v)
    }

    /// A new parent is a move (`/categories/{id}/move`), not a field update.
    pub(super) async fn update_category_impl(
        &self,
        id: &str,
        c: &CategoryPatch,
    ) -> Result<Category, ConnectorError> {
        let id = entity_id("category", id)?;
        if c.name.is_some() || c.description.is_some() {
            let mut category = Map::new();
            category.insert("id".into(), json!(id.parse::<u64>().unwrap_or(0)));
            if let Some(n) = &c.name {
                category.insert("name".into(), json!(n.trim()));
            }
            if let Some(d) = &c.description {
                category.insert(
                    "custom_attributes".into(),
                    json!([{"attribute_code": "description", "value": d}]),
                );
            }
            self.write(
                Method::Put,
                "all/",
                &format!("/categories/{id}"),
                Some(json!({"category": category})),
            )
            .await?;
        }
        if let Some(parent) = &c.parent_id {
            let parent: u64 = entity_id("category", parent)?.parse().unwrap_or(0);
            self.write(
                Method::Put,
                "",
                &format!("/categories/{id}/move"),
                Some(json!({"parentId": parent})),
            )
            .await?;
        }
        self.category(id).await
    }

    async fn default_root_category(&self) -> Result<String, ConnectorError> {
        let groups = self.get("/store/storeGroups", &[]).await?;
        groups
            .as_array()
            .into_iter()
            .flatten()
            .filter(|g| g["id"].as_u64().is_some_and(|id| id != 0))
            .find_map(|g| types::string(&g["root_category_id"]))
            .ok_or_else(|| bad_request("no store group with a root category; give a parent_id"))
    }
}
