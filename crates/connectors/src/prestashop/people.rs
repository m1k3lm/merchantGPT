//! Customers and their addresses.

use std::collections::BTreeSet;

use serde_json::Value;

use super::xml::{self, Field};
use super::{bad_request, list, literal, numeric_id, one_of, types, PrestaShop};
use crate::domain::*;
use crate::{shop::unsupported, ConnectorError, Platform};

pub(super) const CUSTOMER_FIELDS: &str = "[id,email,firstname,lastname,newsletter]";
/// PrestaShop's standard "Customer" group, created at install.
const CUSTOMER_GROUP: &str = "3";

struct Country {
    id: String,
    contains_states: bool,
    needs_id_number: bool,
}

fn random_password() -> String {
    let mut bytes = [0u8; 18];
    getrandom::fill(&mut bytes).expect("OS random source available");
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    // Upper case and a symbol keep it valid under strict password policies.
    format!("Mg{hex}!")
}

impl PrestaShop {
    pub(super) async fn customer_by_id(&self, id: &str) -> Result<Customer, ConnectorError> {
        let id = numeric_id("customer", id)?;
        let v = self
            .get(
                "customers",
                vec![
                    ("display".into(), CUSTOMER_FIELDS.into()),
                    ("filter[id]".into(), format!("[{id}]")),
                ],
            )
            .await?;
        let raw = list(&v, "customers")
            .into_iter()
            .next()
            .ok_or_else(|| ConnectorError::NotFound(format!("customer {id}")))?;
        let mut customer = types::customer(&raw)?;
        // PrestaShop keeps phone numbers on addresses.
        customer.phone = self
            .addresses_of(id)
            .await
            .ok()
            .and_then(|a| a.into_iter().find_map(|a| a.phone));
        Ok(customer)
    }

    pub(super) async fn customer_by_email(
        &self,
        email: &str,
    ) -> Result<Option<String>, ConnectorError> {
        let v = self
            .get(
                "customers",
                vec![
                    ("display".into(), "[id]".into()),
                    ("filter[email]".into(), format!("[{}]", literal(email))),
                    ("filter[deleted]".into(), "[0]".into()),
                ],
            )
            .await?;
        Ok(list(&v, "customers")
            .first()
            .and_then(|c| types::string(&c["id"])))
    }

    pub(super) async fn create_customer_in(
        &self,
        c: &CustomerPatch,
    ) -> Result<Customer, ConnectorError> {
        let (Some(first), Some(last)) = (&c.first_name, &c.last_name) else {
            return Err(bad_request(
                "PrestaShop needs first_name and last_name for a customer",
            ));
        };
        if c.phone.is_some() {
            return Err(bad_request(
                "PrestaShop keeps phone numbers on addresses; create the customer, then an address with the phone",
            ));
        }
        let email = c.email.as_deref().unwrap_or_default().trim();
        let fields = vec![
            ("email", Field::Text(email.into())),
            ("firstname", Field::Text(first.trim().into())),
            ("lastname", Field::Text(last.trim().into())),
            // The customer sets their own through "forgot password".
            ("passwd", Field::Text(random_password())),
            (
                "newsletter",
                Field::Text(u8::from(c.accepts_marketing.unwrap_or(false)).to_string()),
            ),
            ("active", Field::Text("1".into())),
            ("id_default_group", Field::Text(CUSTOMER_GROUP.into())),
            (
                "associations",
                Field::Xml(xml::rows(
                    "groups",
                    "group",
                    &[vec![("id", CUSTOMER_GROUP.to_string())]],
                )),
            ),
        ];
        let v = self
            .post("customers", xml::document("customer", &fields))
            .await?;
        let id = Self::created_id(&v, "customer")?;
        self.customer_by_id(&id).await
    }

    pub(super) async fn update_customer_in(
        &self,
        id: &str,
        c: &CustomerPatch,
    ) -> Result<Customer, ConnectorError> {
        let id = numeric_id("customer", id)?;
        if c.phone.is_some() {
            return Err(bad_request(
                "PrestaShop keeps phone numbers on addresses; update the customer's address instead",
            ));
        }
        let mut fields = vec![("id", Field::Text(id.into()))];
        if let Some(e) = &c.email {
            fields.push(("email", Field::Text(e.trim().into())));
        }
        if let Some(f) = &c.first_name {
            fields.push(("firstname", Field::Text(f.trim().into())));
        }
        if let Some(l) = &c.last_name {
            fields.push(("lastname", Field::Text(l.trim().into())));
        }
        if let Some(m) = c.accepts_marketing {
            fields.push(("newsletter", Field::Text(u8::from(m).to_string())));
        }
        self.patch(
            &format!("customers/{id}"),
            xml::document("customer", &fields),
        )
        .await?;
        self.customer_by_id(id).await
    }

    pub(super) async fn delete_customer_by_id(&self, id: &str) -> Result<(), ConnectorError> {
        let id = numeric_id("customer", id)?;
        self.delete(&format!("customers/{id}")).await
    }

    async fn places(&self, raw: &[Value]) -> types::Places {
        let ids = |k: &str| -> BTreeSet<String> {
            raw.iter()
                .filter_map(|a| types::string(&a[k]))
                .filter(|v| v != "0")
                .collect()
        };
        let mut places = types::Places::default();
        for (resource, key, map) in [
            ("countries", "id_country", &mut places.countries),
            ("states", "id_state", &mut places.states),
        ] {
            let wanted = ids(key);
            if wanted.is_empty() {
                continue;
            }
            let params = vec![
                ("display".into(), "[id,iso_code]".into()),
                ("filter[id]".into(), one_of(wanted)),
            ];
            // Without access the ids are shown instead of ISO codes.
            if let Ok(v) = self.get(resource, params).await {
                for row in list(&v, resource) {
                    if let (Some(id), Some(iso)) =
                        (types::string(&row["id"]), types::string(&row["iso_code"]))
                    {
                        map.insert(id, iso);
                    }
                }
            }
        }
        places
    }

    async fn address_rows(&self, filter: (&str, String)) -> Result<Vec<Value>, ConnectorError> {
        let v = self
            .get(
                "addresses",
                vec![
                    ("display".into(), "full".into()),
                    (format!("filter[{}]", filter.0), filter.1),
                    ("filter[deleted]".into(), "[0]".into()),
                ],
            )
            .await?;
        Ok(list(&v, "addresses"))
    }

    pub(super) async fn addresses_of(
        &self,
        customer_id: &str,
    ) -> Result<Vec<Address>, ConnectorError> {
        let customer_id = numeric_id("customer", customer_id)?;
        let raw = self
            .address_rows(("id_customer", format!("[{customer_id}]")))
            .await?;
        let places = self.places(&raw).await;
        raw.iter().map(|a| types::address(a, &places)).collect()
    }

    /// The address row, checked to belong to the customer.
    pub(super) async fn address_row(
        &self,
        customer_id: &str,
        id: &str,
    ) -> Result<Value, ConnectorError> {
        let id = numeric_id("address", id)?;
        let raw = self.address_rows(("id", format!("[{id}]"))).await?;
        raw.into_iter()
            .find(|a| types::string(&a["id_customer"]).as_deref() == Some(customer_id.trim()))
            .ok_or_else(|| {
                ConnectorError::NotFound(format!("address {id} of customer {customer_id}"))
            })
    }

    async fn address_by_id(&self, id: &str) -> Result<Address, ConnectorError> {
        let raw = self.address_rows(("id", format!("[{id}]"))).await?;
        let places = self.places(&raw).await;
        raw.first()
            .map(|a| types::address(a, &places))
            .transpose()?
            .ok_or_else(|| ConnectorError::NotFound(format!("address {id}")))
    }

    async fn country(&self, iso: &str) -> Result<Country, ConnectorError> {
        let iso = literal(iso).to_ascii_uppercase();
        let v = self
            .get(
                "countries",
                vec![
                    (
                        "display".into(),
                        "[id,contains_states,need_identification_number]".into(),
                    ),
                    ("filter[iso_code]".into(), format!("[{iso}]")),
                ],
            )
            .await?;
        let row = list(&v, "countries")
            .into_iter()
            .next()
            .ok_or_else(|| bad_request(format!("PrestaShop has no country with ISO code {iso}")))?;
        Ok(Country {
            id: types::string(&row["id"]).unwrap_or_default(),
            contains_states: types::int(&row["contains_states"]) == Some(1),
            needs_id_number: types::int(&row["need_identification_number"]) == Some(1),
        })
    }

    /// Matches `ES-M`, `M` or the state's name.
    async fn state(&self, country: &Country, state: &str) -> Result<String, ConnectorError> {
        let v = self
            .get(
                "states",
                vec![
                    ("display".into(), "[id,iso_code,name]".into()),
                    ("filter[id_country]".into(), format!("[{}]", country.id)),
                ],
            )
            .await?;
        let wanted = state.trim().to_lowercase();
        let rows = list(&v, "states");
        rows.iter()
            .find(|s| {
                let iso = types::string(&s["iso_code"])
                    .unwrap_or_default()
                    .to_lowercase();
                let name = types::string(&s["name"]).unwrap_or_default().to_lowercase();
                iso == wanted || iso.rsplit('-').next() == Some(wanted.as_str()) || name == wanted
            })
            .and_then(|s| types::string(&s["id"]))
            .ok_or_else(|| {
                let names: Vec<String> = rows
                    .iter()
                    .take(8)
                    .filter_map(|s| types::string(&s["name"]))
                    .collect();
                bad_request(format!(
                    "no state {state:?} in this country; states include {}",
                    names.join(", ")
                ))
            })
    }

    async fn place_fields(
        &self,
        country: &str,
        state: Option<&str>,
    ) -> Result<Vec<(&'static str, Field)>, ConnectorError> {
        let c = self.country(country).await?;
        if c.needs_id_number {
            return Err(unsupported(
                Platform::PrestaShop,
                &format!(
                    "addresses in {} without an identification number (DNI), which this shop requires there; add it in the back office or with shop_api_write and a <dni> field",
                    country.trim().to_uppercase()
                ),
            ));
        }
        let mut fields = vec![("id_country", Field::Text(c.id.clone()))];
        match (c.contains_states, state.filter(|s| !s.trim().is_empty())) {
            (true, Some(s)) => fields.push(("id_state", Field::Text(self.state(&c, s).await?))),
            (true, None) => {
                return Err(bad_request(format!(
                    "addresses in {} need a state",
                    country.trim().to_uppercase()
                )))
            }
            (false, _) => fields.push(("id_state", Field::Text("0".into()))),
        }
        Ok(fields)
    }

    fn text_fields(a: &AddressPatch) -> Vec<(&'static str, Field)> {
        [
            ("alias", &a.kind),
            ("firstname", &a.first_name),
            ("lastname", &a.last_name),
            ("company", &a.company),
            ("address1", &a.address1),
            ("address2", &a.address2),
            ("city", &a.city),
            ("postcode", &a.postcode),
            ("phone", &a.phone),
        ]
        .into_iter()
        .filter_map(|(k, v)| v.as_ref().map(|v| (k, Field::Text(v.trim().to_string()))))
        .collect()
    }

    pub(super) async fn create_address_for(
        &self,
        customer_id: &str,
        a: &AddressPatch,
    ) -> Result<Address, ConnectorError> {
        let customer_id = numeric_id("customer", customer_id)?;
        let country = a
            .country
            .as_deref()
            .ok_or_else(|| bad_request("country (ISO code) is required"))?;
        if a.address1.is_none() || a.city.is_none() {
            return Err(bad_request("address1 and city are required"));
        }
        let mut filled = a.clone();
        if filled.first_name.is_none() || filled.last_name.is_none() {
            let c = self.customer_by_id(customer_id).await?;
            filled.first_name.get_or_insert(c.first_name);
            filled.last_name.get_or_insert(c.last_name);
        }
        filled.kind.get_or_insert_with(|| "Address".into());
        let mut fields = vec![("id_customer", Field::Text(customer_id.into()))];
        fields.extend(Self::text_fields(&filled));
        fields.extend(self.place_fields(country, a.state.as_deref()).await?);
        let v = self
            .post("addresses", xml::document("address", &fields))
            .await?;
        let id = Self::created_id(&v, "address")?;
        self.address_by_id(&id).await
    }

    pub(super) async fn update_address_for(
        &self,
        customer_id: &str,
        id: &str,
        a: &AddressPatch,
    ) -> Result<Address, ConnectorError> {
        let row = self.address_row(customer_id, id).await?;
        let id = types::string(&row["id"]).unwrap_or_default();
        let mut fields = vec![("id", Field::Text(id.clone()))];
        fields.extend(Self::text_fields(a));
        if let Some(country) = &a.country {
            fields.extend(self.place_fields(country, a.state.as_deref()).await?);
        } else if let Some(state) = &a.state {
            let iso = self
                .places(std::slice::from_ref(&row))
                .await
                .countries
                .get(&types::string(&row["id_country"]).unwrap_or_default())
                .cloned()
                .ok_or_else(|| bad_request("give the country together with the state"))?;
            fields.extend(self.place_fields(&iso, Some(state)).await?);
        }
        self.patch(
            &format!("addresses/{id}"),
            xml::document("address", &fields),
        )
        .await?;
        self.address_by_id(&id).await
    }

    pub(super) async fn delete_address_for(
        &self,
        customer_id: &str,
        id: &str,
    ) -> Result<(), ConnectorError> {
        let row = self.address_row(customer_id, id).await?;
        let id = types::string(&row["id"]).unwrap_or_default();
        self.delete(&format!("addresses/{id}")).await
    }
}
