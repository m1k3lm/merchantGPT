//! Coupons: a Magento coupon belongs to a cart price rule (`salesRules`),
//! which holds the discount. Each coupon written here gets its own rule with
//! one specific code.

use serde_json::{json, Value};

use super::criteria::{Criteria, Filter};
use super::{bad_request, entity_id, types, Magento};
use crate::domain::*;
use crate::transport::Method;
use crate::ConnectorError;

const COMBINE: &str = "Magento\\SalesRule\\Model\\Rule\\Condition\\Combine";
const ADDRESS: &str = "Magento\\SalesRule\\Model\\Rule\\Condition\\Address";

/// "Subtotal is at least `amount`", in the shape `types::min_subtotal` reads.
fn min_subtotal_condition(amount: &rust_decimal::Decimal) -> Value {
    json!({
        "condition_type": COMBINE,
        "aggregator_type": "all",
        "value": "1",
        "conditions": [{
            "condition_type": ADDRESS,
            "attribute_name": "base_subtotal",
            "operator": ">=",
            "value": amount.to_string()
        }]
    })
}

/// Writes the patch's rule-level fields into a rule object.
fn apply_rule(rule: &mut Value, c: &CouponPatch) -> Result<(), ConnectorError> {
    if let Some(kind) = &c.kind {
        rule["simple_action"] = json!(types::simple_action(kind)?);
    }
    if let Some(amount) = &c.amount {
        rule["discount_amount"] = json!(amount.to_string().parse::<f64>().unwrap_or(0.0));
    }
    if let Some(d) = c.expires_on {
        rule["to_date"] = json!(d.to_string());
    }
    if let Some(limit) = c.usage_limit {
        rule["uses_per_coupon"] = json!(limit);
    }
    if let Some(active) = c.active {
        rule["is_active"] = json!(active);
    }
    if let Some(d) = &c.description {
        rule["description"] = json!(d);
    }
    if let Some(min) = &c.minimum_amount {
        rule["condition"] = min_subtotal_condition(min);
    }
    Ok(())
}

impl Magento {
    async fn rules(&self, ids: &[String]) -> Result<Vec<Value>, ConnectorError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let c = Criteria::page(1, ids.len() as u32).and(vec![Filter::cond(
            "rule_id",
            "in",
            ids.join(","),
        )]);
        let v = self.get("/salesRules/search", &c.params()).await?;
        Ok(v["items"].as_array().cloned().unwrap_or_default())
    }

    fn join(coupon: &Value, rules: &[Value]) -> Result<Coupon, ConnectorError> {
        let rule_id = types::string(&coupon["rule_id"]);
        let rule = rules
            .iter()
            .find(|r| types::string(&r["rule_id"]) == rule_id)
            .cloned()
            .unwrap_or(Value::Null);
        types::coupon(coupon, &rule)
    }

    async fn coupon_raw(&self, id: &str) -> Result<Value, ConnectorError> {
        let id = entity_id("coupon", id)?;
        let c = Criteria::page(1, 1).and(vec![Filter::eq("coupon_id", id)]);
        let v = self.get("/coupons/search", &c.params()).await?;
        v["items"]
            .as_array()
            .and_then(|items| items.first())
            .cloned()
            .ok_or_else(|| ConnectorError::NotFound(format!("coupon {id}")))
    }

    async fn coupon_with_rule(&self, coupon: &Value) -> Result<Coupon, ConnectorError> {
        let rule_id = types::string(&coupon["rule_id"]).unwrap_or_default();
        Self::join(coupon, &self.rules(&[rule_id]).await?)
    }

    pub(super) async fn list_coupons_impl(
        &self,
        q: &CouponQuery,
    ) -> Result<Page<Coupon>, ConnectorError> {
        let mut c = Self::criteria(q.page, q.per_page);
        if let Some(code) = q.code.as_deref().filter(|s| !s.trim().is_empty()) {
            c = c.and(vec![Filter::eq("code", code.trim())]);
        }
        let coupons = self.list("/coupons/search", c, |v| Ok(v.clone())).await?;
        let mut ids: Vec<String> = coupons
            .items
            .iter()
            .filter_map(|c| types::string(&c["rule_id"]))
            .collect();
        ids.sort();
        ids.dedup();
        let rules = self.rules(&ids).await?;
        Ok(Page {
            items: coupons
                .items
                .iter()
                .map(|c| Self::join(c, &rules))
                .collect::<Result<_, _>>()?,
            page: coupons.page,
            total_pages: coupons.total_pages,
            total: coupons.total,
        })
    }

    /// For every website and customer group, so the code works for anyone.
    pub(super) async fn create_coupon_impl(
        &self,
        c: &CouponPatch,
    ) -> Result<Coupon, ConnectorError> {
        let code = c
            .code
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| bad_request("a coupon needs a code"))?;
        if c.kind.is_none() || c.amount.is_none() {
            return Err(bad_request("a coupon needs a kind and an amount"));
        }
        let groups = self
            .get("/customerGroups/search", &Criteria::page(1, 100).params())
            .await?;
        let group_ids: Vec<u64> = groups["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|g| g["id"].as_u64())
            .collect();
        let mut rule = json!({
            "name": format!("Coupon {code}"),
            "website_ids": self.website_ids().await?,
            "customer_group_ids": group_ids,
            "is_active": true,
            "uses_per_customer": 0,
            "stop_rules_processing": false,
            "coupon_type": "SPECIFIC_COUPON",
            "use_auto_generation": false
        });
        apply_rule(&mut rule, c)?;
        let saved = self
            .write(
                Method::Post,
                "",
                "/salesRules",
                Some(json!({ "rule": rule })),
            )
            .await?;
        let rule_id = types::string(&saved["rule_id"])
            .ok_or_else(|| ConnectorError::InvalidResponse("rule without rule_id".into()))?;
        let coupon = json!({"coupon": {
            "rule_id": rule_id.parse::<u64>().unwrap_or(0),
            "code": code,
            "usage_limit": c.usage_limit.unwrap_or(0),
            "usage_per_customer": 0,
            "is_primary": true,
            "type": 0
        }});
        match self.write(Method::Post, "", "/coupons", Some(coupon)).await {
            Ok(created) => types::coupon(&created, &saved),
            Err(e) => {
                // A rule without its code would never apply; do not leave it.
                let _ = self
                    .write(Method::Delete, "", &format!("/salesRules/{rule_id}"), None)
                    .await;
                Err(e)
            }
        }
    }

    pub(super) async fn update_coupon_impl(
        &self,
        id: &str,
        c: &CouponPatch,
    ) -> Result<Coupon, ConnectorError> {
        let mut coupon = self.coupon_raw(id).await?;
        let rule_id = types::string(&coupon["rule_id"])
            .ok_or_else(|| ConnectorError::InvalidResponse("coupon without rule_id".into()))?;
        let rule_fields = c.kind.is_some()
            || c.amount.is_some()
            || c.expires_on.is_some()
            || c.usage_limit.is_some()
            || c.active.is_some()
            || c.description.is_some()
            || c.minimum_amount.is_some();
        if rule_fields {
            let mut rule = self.get(&format!("/salesRules/{rule_id}"), &[]).await?;
            apply_rule(&mut rule, c)?;
            self.write(
                Method::Put,
                "",
                &format!("/salesRules/{rule_id}"),
                Some(json!({ "rule": rule })),
            )
            .await?;
        }
        if c.code.is_some() || c.usage_limit.is_some() {
            if let Some(code) = &c.code {
                coupon["code"] = json!(code.trim());
            }
            if let Some(limit) = c.usage_limit {
                coupon["usage_limit"] = json!(limit);
            }
            let coupon_id = types::string(&coupon["coupon_id"]).unwrap_or_default();
            self.write(
                Method::Put,
                "",
                &format!("/coupons/{coupon_id}"),
                Some(json!({ "coupon": coupon })),
            )
            .await?;
        }
        let fresh = self.coupon_raw(id).await?;
        self.coupon_with_rule(&fresh).await
    }

    /// A primary coupon goes with its rule; any other coupon alone.
    pub(super) async fn delete_coupon_impl(&self, id: &str) -> Result<(), ConnectorError> {
        let coupon = self.coupon_raw(id).await?;
        let path = if types::truthy(&coupon["is_primary"]) {
            format!(
                "/salesRules/{}",
                types::string(&coupon["rule_id"]).unwrap_or_default()
            )
        } else {
            format!(
                "/coupons/{}",
                types::string(&coupon["coupon_id"]).unwrap_or_default()
            )
        };
        self.write(Method::Delete, "", &path, None)
            .await
            .map(|_| ())
    }
}
