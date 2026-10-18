//! Customers and their addresses. Magento saves addresses as part of the
//! customer, so address writes re-save the whole customer.

use serde_json::{json, Map, Value};

use super::{bad_request, entity_id, types, Magento};
use crate::domain::*;
use crate::transport::Method;
use crate::ConnectorError;

/// A resolved region: Magento needs the id for countries with a region list.
pub(super) struct Region {
    pub id: Option<u64>,
    pub code: Option<String>,
    pub name: String,
}

impl Magento {
    pub(super) async fn customer_raw(&self, id: &str) -> Result<Value, ConnectorError> {
        let id = entity_id("customer", id)?;
        self.get(&format!("/customers/{id}"), &[]).await
    }

    async fn save_customer(&self, id: &str, customer: Value) -> Result<Value, ConnectorError> {
        self.write(
            Method::Put,
            "",
            &format!("/customers/{id}"),
            Some(json!({ "customer": customer })),
        )
        .await
    }

    /// Created without a password; the customer sets one through "Forgot
    /// your password".
    pub(super) async fn create_customer_impl(
        &self,
        c: &CustomerPatch,
    ) -> Result<Customer, ConnectorError> {
        if c.phone.is_some() {
            return Err(bad_request(
                "Magento keeps phone numbers on addresses; create the customer, then add an address with the phone",
            ));
        }
        let mut customer = Map::new();
        for (key, value) in [
            ("email", &c.email),
            ("firstname", &c.first_name),
            ("lastname", &c.last_name),
        ] {
            if let Some(v) = value {
                customer.insert(key.into(), json!(v.trim()));
            }
        }
        if let Some(s) = c.accepts_marketing {
            customer.insert("extension_attributes".into(), json!({ "is_subscribed": s }));
        }
        let v = self
            .write(
                Method::Post,
                "",
                "/customers",
                Some(json!({ "customer": customer })),
            )
            .await?;
        types::customer(&v)
    }

    /// Magento's PUT replaces the customer, so the saved one is fetched and
    /// changed in place. A phone goes on the default billing address.
    pub(super) async fn update_customer_impl(
        &self,
        id: &str,
        c: &CustomerPatch,
    ) -> Result<Customer, ConnectorError> {
        let id = entity_id("customer", id)?;
        let mut customer = self.customer_raw(id).await?;
        for (key, value) in [
            ("email", &c.email),
            ("firstname", &c.first_name),
            ("lastname", &c.last_name),
        ] {
            if let Some(v) = value {
                customer[key] = json!(v.trim());
            }
        }
        if let Some(s) = c.accepts_marketing {
            if !customer["extension_attributes"].is_object() {
                customer["extension_attributes"] = json!({});
            }
            customer["extension_attributes"]["is_subscribed"] = json!(s);
        }
        if let Some(phone) = &c.phone {
            let billing = types::string(&customer["default_billing"]);
            let addresses = customer["addresses"]
                .as_array_mut()
                .filter(|a| !a.is_empty())
                .ok_or_else(|| {
                    bad_request(
                        "Magento keeps phone numbers on addresses and this customer has none; add an address with the phone",
                    )
                })?;
            let idx = addresses
                .iter()
                .position(|a| billing.is_some() && types::string(&a["id"]) == billing)
                .unwrap_or(0);
            addresses[idx]["telephone"] = json!(phone.trim());
        }
        types::customer(&self.save_customer(id, customer).await?)
    }

    pub(super) async fn create_address_impl(
        &self,
        customer_id: &str,
        a: &AddressPatch,
    ) -> Result<Address, ConnectorError> {
        let id = entity_id("customer", customer_id)?;
        let mut customer = self.customer_raw(id).await?;
        let before: Vec<Option<String>> = addresses(&customer)
            .iter()
            .map(|a| types::string(&a["id"]))
            .collect();
        let mut new = Map::new();
        new.insert("customer_id".into(), json!(id.parse::<u64>().unwrap_or(0)));
        self.apply_address(&mut new, a).await?;
        claim_defaults(&mut customer, &new);
        if !customer["addresses"].is_array() {
            customer["addresses"] = json!([]);
        }
        if let Some(list) = customer["addresses"].as_array_mut() {
            list.push(Value::Object(new));
        }
        let saved = self.save_customer(id, customer).await?;
        addresses(&saved)
            .iter()
            .find(|x| !before.contains(&types::string(&x["id"])))
            .map(types::address)
            .unwrap_or_else(|| {
                Err(ConnectorError::InvalidResponse(
                    "the new address was not saved".into(),
                ))
            })
    }

    pub(super) async fn update_address_impl(
        &self,
        customer_id: &str,
        address_id: &str,
        a: &AddressPatch,
    ) -> Result<Address, ConnectorError> {
        let id = entity_id("customer", customer_id)?;
        let address_id = entity_id("address", address_id)?;
        let mut customer = self.customer_raw(id).await?;
        let idx = address_index(&customer, address_id)?;
        let mut current = customer["addresses"][idx]
            .as_object()
            .cloned()
            .unwrap_or_default();
        self.apply_address(&mut current, a).await?;
        claim_defaults(&mut customer, &current);
        customer["addresses"][idx] = Value::Object(current);
        let saved = self.save_customer(id, customer).await?;
        let found = addresses(&saved)
            .iter()
            .find(|x| types::string(&x["id"]).as_deref() == Some(address_id))
            .cloned()
            .ok_or_else(|| ConnectorError::NotFound(format!("address {address_id}")))?;
        types::address(&found)
    }

    /// Checks the address belongs to the customer before deleting it.
    pub(super) async fn delete_address_impl(
        &self,
        customer_id: &str,
        address_id: &str,
    ) -> Result<(), ConnectorError> {
        let address_id = entity_id("address", address_id)?;
        let customer = self.customer_raw(customer_id).await?;
        address_index(&customer, address_id)?;
        self.write(
            Method::Delete,
            "",
            &format!("/addresses/{address_id}"),
            None,
        )
        .await
        .map(|_| ())
    }

    /// Writes the patch into a customer address object (snake_case, region
    /// as an object).
    async fn apply_address(
        &self,
        target: &mut Map<String, Value>,
        a: &AddressPatch,
    ) -> Result<(), ConnectorError> {
        for (key, value) in [
            ("firstname", &a.first_name),
            ("lastname", &a.last_name),
            ("company", &a.company),
            ("city", &a.city),
            ("postcode", &a.postcode),
            ("telephone", &a.phone),
            ("vat_id", &a.id_number),
        ] {
            if let Some(v) = value {
                target.insert(key.into(), json!(v.trim()));
            }
        }
        if a.address1.is_some() || a.address2.is_some() {
            let old: Vec<String> = target
                .get("street")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(types::string)
                .collect();
            let line1 = a.address1.clone().or_else(|| old.first().cloned());
            let line2 = a.address2.clone().or_else(|| old.get(1).cloned());
            let street: Vec<String> = [line1, line2]
                .into_iter()
                .flatten()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            target.insert("street".into(), json!(street));
        }
        if let Some(c) = &a.country {
            target.insert("country_id".into(), json!(c.trim().to_ascii_uppercase()));
        }
        if a.state.is_some() || a.country.is_some() {
            let country = target
                .get("country_id")
                .and_then(types::string)
                .ok_or_else(|| bad_request("an address needs a country (ISO code, e.g. ES)"))?;
            let state = a.state.clone().or_else(|| {
                types::string(&target.get("region").cloned().unwrap_or_default()["region"])
            });
            match self.region(&country, state.as_deref()).await? {
                Some(r) => {
                    target.insert(
                        "region".into(),
                        json!({"region": r.name, "region_code": r.code, "region_id": r.id}),
                    );
                    if let Some(id) = r.id {
                        target.insert("region_id".into(), json!(id));
                    }
                }
                None => {
                    target.remove("region");
                    target.remove("region_id");
                }
            }
        }
        if let Some(kind) = &a.kind {
            let kind = kind.to_ascii_lowercase();
            target.insert("default_billing".into(), json!(kind.contains("billing")));
            target.insert("default_shipping".into(), json!(kind.contains("shipping")));
        }
        Ok(())
    }

    /// Matches a state by code or name against the country's region list;
    /// a country without a list takes the name as free text.
    pub(super) async fn region(
        &self,
        country: &str,
        state: Option<&str>,
    ) -> Result<Option<Region>, ConnectorError> {
        let Some(state) = state.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let info = self
            .get(&format!("/directory/countries/{}", country.trim()), &[])
            .await?;
        let regions = info["available_regions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if regions.is_empty() {
            return Ok(Some(Region {
                id: None,
                code: None,
                name: state.to_string(),
            }));
        }
        let found = regions.iter().find(|r| {
            [&r["code"], &r["name"]]
                .iter()
                .filter_map(|v| v.as_str())
                .any(|v| v.eq_ignore_ascii_case(state))
        });
        match found {
            Some(r) => Ok(Some(Region {
                id: types::string(&r["id"]).and_then(|id| id.parse().ok()),
                code: types::string(&r["code"]),
                name: types::string(&r["name"]).unwrap_or_else(|| state.to_string()),
            })),
            None => {
                let names: Vec<String> = regions
                    .iter()
                    .filter_map(|r| types::string(&r["name"]))
                    .take(10)
                    .collect();
                Err(bad_request(format!(
                    "{state:?} is not a region of {country}; Magento knows e.g. {}",
                    names.join(", ")
                )))
            }
        }
    }
}

fn addresses(customer: &Value) -> Vec<Value> {
    customer["addresses"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn address_index(customer: &Value, address_id: &str) -> Result<usize, ConnectorError> {
    addresses(customer)
        .iter()
        .position(|a| types::string(&a["id"]).as_deref() == Some(address_id))
        .ok_or_else(|| ConnectorError::NotFound(format!("address {address_id} of this customer")))
}

/// One address per default: claiming one clears the flag on the others.
fn claim_defaults(customer: &mut Value, claimant: &Map<String, Value>) {
    let id = claimant.get("id").and_then(types::string);
    let Some(list) = customer["addresses"].as_array_mut() else {
        return;
    };
    for flag in ["default_billing", "default_shipping"] {
        if claimant.get(flag).is_some_and(types::truthy) {
            for other in list.iter_mut() {
                if id.is_none() || types::string(&other["id"]) != id {
                    other[flag] = json!(false);
                }
            }
        }
    }
}
