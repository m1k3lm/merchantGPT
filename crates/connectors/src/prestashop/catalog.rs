//! Products created or deleted, product categories, and category CRUD.

use rust_decimal::Decimal;
use serde_json::Value;

use super::xml::{self, Field};
use super::{bad_request, list, literal, numeric_id, page, paging, types, Params, PrestaShop};
use crate::domain::*;
use crate::{ConnectorError, ShopConnector};

const CATEGORY_FIELDS: &str = "[id,id_parent,name,description,active]";

impl PrestaShop {
    /// The full single record, for multilang texts and associations the
    /// list endpoint does not return.
    pub(super) async fn product_texts(&self, id: &str) -> Result<Value, ConnectorError> {
        let id = numeric_id("product", id)?;
        Ok(self.get(&format!("products/{id}"), vec![]).await?["product"].clone())
    }

    pub(super) async fn product_categories(&self, id: &str) -> Result<Vec<String>, ConnectorError> {
        let p = self.product_texts(id).await?;
        Ok(p["associations"]["categories"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| types::string(&c["id"]))
            .collect())
    }

    /// `id_category_default` (the first id) plus the category associations,
    /// which replace the product's current ones.
    pub(super) async fn category_fields(
        &self,
        ids: &[String],
    ) -> Result<Vec<(&'static str, Field)>, ConnectorError> {
        let ids: Vec<String> = ids
            .iter()
            .map(|c| numeric_id("category", c).map(str::to_string))
            .collect::<Result<_, _>>()?;
        let Some(first) = ids.first().cloned() else {
            return Err(bad_request(
                "a PrestaShop product needs at least one category",
            ));
        };
        let rows: Vec<Vec<(&str, String)>> = ids.into_iter().map(|id| vec![("id", id)]).collect();
        Ok(vec![
            ("id_category_default", Field::Text(first)),
            (
                "associations",
                Field::Xml(xml::rows("categories", "category", &rows)),
            ),
        ])
    }

    /// The shop's "Home" category, where new products and categories go by default.
    async fn home_category(&self) -> Result<String, ConnectorError> {
        let v = self
            .get(
                "categories",
                vec![
                    ("display".into(), "[id]".into()),
                    ("filter[is_root_category]".into(), "[1]".into()),
                ],
            )
            .await?;
        Ok(list(&v, "categories")
            .first()
            .and_then(|c| types::string(&c["id"]))
            .unwrap_or_else(|| "2".into()))
    }

    pub(super) async fn set_product_stock(
        &self,
        id: &str,
        quantity: i64,
    ) -> Result<(), ConnectorError> {
        let rows = self.stock(&[id.to_string()]).await?;
        let Some((row, _)) = rows.get(id) else {
            return Err(bad_request("PrestaShop has no stock row for this product"));
        };
        let doc = xml::document(
            "stock_available",
            &[
                ("id", Field::Text(row.clone())),
                ("quantity", Field::Text(quantity.to_string())),
            ],
        );
        self.patch(&format!("stock_availables/{row}"), doc)
            .await
            .map(|_| ())
    }

    /// A standard product (no combinations) in the given categories, or in
    /// Home. Prices exclude tax.
    pub(super) async fn create_product_in(
        &self,
        p: &ProductPatch,
    ) -> Result<Product, ConnectorError> {
        let name = p.name.as_deref().unwrap_or_default().trim().to_string();
        let regular = p.regular_price.unwrap_or_default();
        if let Some(s) = p.sale_price.filter(|s| !s.is_zero() && *s >= regular) {
            return Err(bad_request(format!(
                "the sale price {s} must be below the regular price {regular}"
            )));
        }
        let active = match &p.status {
            Some(st) => types::active(st)?,
            None => true,
        };
        let categories = match &p.category_ids {
            Some(c) if !c.is_empty() => c.clone(),
            _ => vec![self.home_category().await?],
        };
        let mut fields = vec![
            ("name", self.every_language(&name).await?),
            (
                "link_rewrite",
                self.every_language(&types::slug(&name)).await?,
            ),
            ("price", Field::Text(regular.to_string())),
            ("active", Field::Text(u8::from(active).to_string())),
            ("state", Field::Text("1".into())),
            ("product_type", Field::Text("standard".into())),
        ];
        if let Some(sku) = &p.sku {
            fields.push(("reference", Field::Text(sku.trim().into())));
        }
        if let Some(d) = &p.description {
            fields.push(("description", self.every_language(d).await?));
        }
        fields.extend(self.category_fields(&categories).await?);
        let v = self
            .post("products", xml::document("product", &fields))
            .await?;
        let id = Self::created_id(&v, "product")?;
        if let Some(sale) = p.sale_price.filter(|s| !s.is_zero()) {
            self.set_sale(&id, regular, sale).await?;
        }
        if let Some(q) = p.stock_quantity {
            self.set_product_stock(&id, q).await?;
        }
        self.get_product(&id).await
    }

    pub(super) async fn delete_product_by_id(&self, id: &str) -> Result<(), ConnectorError> {
        let id = numeric_id("product", id)?;
        self.delete(&format!("products/{id}")).await
    }

    async fn category(&self, id: &str) -> Result<(Value, Category), ConnectorError> {
        let id = numeric_id("category", id)?;
        let raw = self.get(&format!("categories/{id}"), vec![]).await?["category"].clone();
        let category = types::category(&raw)?;
        Ok((raw, category))
    }

    pub(super) async fn categories_page(
        &self,
        q: &CategoryQuery,
    ) -> Result<Page<Category>, ConnectorError> {
        let mut params: Params = vec![
            ("display".into(), CATEGORY_FIELDS.into()),
            ("sort".into(), "[id_ASC]".into()),
        ];
        if let Some(s) = q.search.as_deref().map(literal).filter(|s| !s.is_empty()) {
            params.push(("filter[name]".into(), format!("%[{s}]%")));
        }
        let (n, size) = paging(&mut params, q.page, q.per_page, super::MAX_PER_PAGE);
        let items = list(&self.get("categories", params).await?, "categories")
            .iter()
            .map(types::category)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(page(items, n, size))
    }

    pub(super) async fn create_category_in(
        &self,
        c: &CategoryPatch,
    ) -> Result<Category, ConnectorError> {
        let name = c.name.as_deref().unwrap_or_default().trim().to_string();
        let parent = match &c.parent_id {
            Some(p) => numeric_id("category", p)?.to_string(),
            None => self.home_category().await?,
        };
        let mut fields = vec![
            ("name", self.every_language(&name).await?),
            (
                "link_rewrite",
                self.every_language(&types::slug(&name)).await?,
            ),
            ("id_parent", Field::Text(parent)),
            ("active", Field::Text("1".into())),
        ];
        if let Some(d) = &c.description {
            fields.push(("description", self.every_language(d).await?));
        }
        let v = self
            .post("categories", xml::document("category", &fields))
            .await?;
        let id = Self::created_id(&v, "category")?;
        Ok(self.category(&id).await?.1)
    }

    pub(super) async fn update_category_in(
        &self,
        id: &str,
        c: &CategoryPatch,
    ) -> Result<Category, ConnectorError> {
        let (raw, current) = self.category(id).await?;
        let mut fields = vec![("id", Field::Text(current.id.clone()))];
        if let Some(name) = &c.name {
            fields.push(("name", self.one_language(&raw["name"], name).await?));
        }
        if let Some(p) = &c.parent_id {
            fields.push(("id_parent", Field::Text(numeric_id("category", p)?.into())));
        }
        if let Some(d) = &c.description {
            fields.push((
                "description",
                self.one_language(&raw["description"], d).await?,
            ));
        }
        self.patch(
            &format!("categories/{}", current.id),
            xml::document("category", &fields),
        )
        .await?;
        Ok(self.category(&current.id).await?.1)
    }

    pub(super) async fn delete_category_by_id(&self, id: &str) -> Result<(), ConnectorError> {
        let id = numeric_id("category", id)?;
        self.delete(&format!("categories/{id}")).await
    }
}

/// `round(unit, 2) * quantity`, as PrestaShop totals a cart line by default.
pub(super) fn line_total(unit: Decimal, quantity: i64) -> Decimal {
    unit.round_dp_with_strategy(2, rust_decimal::RoundingStrategy::MidpointAwayFromZero)
        * Decimal::from(quantity)
}
