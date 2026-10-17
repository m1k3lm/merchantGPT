//! Drafting a Search campaign: validates the merchant's input against Google
//! Ads limits and builds one atomic `googleAds:mutate` request. Campaigns are
//! always created PAUSED, so nothing serves or spends until the merchant
//! enables them in Google Ads.

use std::str::FromStr;

use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CampaignDraft {
    pub name: String,
    /// Average daily budget in the account currency, e.g. "20.00".
    pub daily_budget: Value,
    pub final_url: String,
    pub headlines: Vec<String>,
    pub descriptions: Vec<String>,
    pub keywords: Vec<String>,
    #[serde(default)]
    pub match_type: Option<String>,
    /// Max CPC in the account currency; without it the campaign maximizes clicks.
    #[serde(default)]
    pub max_cpc: Option<Value>,
    /// ISO country codes to target, e.g. ["ES"]; empty means all locations.
    #[serde(default)]
    pub country_codes: Vec<String>,
    pub contains_eu_political_advertising: bool,
    #[serde(default)]
    pub ad_group_name: Option<String>,
}

fn money(v: &Value, what: &str) -> Result<Decimal, String> {
    let text = match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.trim().replace(',', "."),
        _ => return Err(format!("{what} must be an amount like \"20.00\"")),
    };
    let d = Decimal::from_str(&text).map_err(|_| format!("{what} {text:?} is not an amount"))?;
    if d <= Decimal::ZERO {
        return Err(format!("{what} must be greater than zero"));
    }
    Ok(d)
}

/// Amounts in micros, rounded to whole cents (Google requires multiples of 10,000).
fn micros(d: Decimal) -> String {
    let cents = (d * Decimal::from(100)).round();
    (cents * Decimal::from(10_000)).trunc().to_string()
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

impl CampaignDraft {
    /// Checks Google's limits locally so the model gets precise feedback.
    pub fn validate(&self) -> Result<(), String> {
        let mut problems = Vec::new();
        if self.name.trim().is_empty() || chars(&self.name) > 255 {
            problems.push("name must be 1-255 characters".to_string());
        }
        if let Err(e) = money(&self.daily_budget, "daily_budget") {
            problems.push(e);
        }
        if let Some(cpc) = &self.max_cpc {
            if let Err(e) = money(cpc, "max_cpc") {
                problems.push(e);
            }
        }
        let url = self.final_url.trim();
        if !(url.starts_with("https://") || url.starts_with("http://")) || url.contains(' ') {
            problems.push("final_url must be a full http(s) URL of the shop page".into());
        }
        if !(3..=15).contains(&self.headlines.len()) {
            problems.push("provide 3 to 15 headlines".into());
        }
        for h in &self.headlines {
            if h.trim().is_empty() || chars(h) > 30 {
                problems.push(format!(
                    "headline {h:?} must be 1-30 characters (has {})",
                    chars(h)
                ));
            }
        }
        let mut unique = self
            .headlines
            .iter()
            .map(|h| h.trim().to_lowercase())
            .collect::<Vec<_>>();
        unique.sort();
        unique.dedup();
        if unique.len() != self.headlines.len() {
            problems.push("headlines must be different from each other".into());
        }
        if !(2..=4).contains(&self.descriptions.len()) {
            problems.push("provide 2 to 4 descriptions".into());
        }
        for d in &self.descriptions {
            if d.trim().is_empty() || chars(d) > 90 {
                problems.push(format!(
                    "description {d:?} must be 1-90 characters (has {})",
                    chars(d)
                ));
            }
        }
        if self.keywords.is_empty() || self.keywords.len() > 50 {
            problems.push("provide 1 to 50 keywords".into());
        }
        for k in &self.keywords {
            if k.trim().is_empty() || chars(k) > 80 || k.split_whitespace().count() > 10 {
                problems.push(format!(
                    "keyword {k:?} must be 1-80 characters and at most 10 words"
                ));
            }
        }
        if self.match_type().is_none() {
            problems.push("match_type must be BROAD, PHRASE or EXACT".into());
        }
        for c in &self.country_codes {
            if c.len() != 2 || !c.chars().all(|ch| ch.is_ascii_alphabetic()) {
                problems.push(format!("country code {c:?} must be two letters like ES"));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    fn match_type(&self) -> Option<&'static str> {
        match self
            .match_type
            .as_deref()
            .unwrap_or("PHRASE")
            .trim()
            .to_ascii_uppercase()
            .as_str()
        {
            "BROAD" => Some("BROAD"),
            "PHRASE" => Some("PHRASE"),
            "EXACT" => Some("EXACT"),
            _ => None,
        }
    }

    pub fn country_codes(&self) -> Vec<String> {
        let mut c: Vec<String> = self
            .country_codes
            .iter()
            .map(|c| c.trim().to_ascii_uppercase())
            .collect();
        c.sort();
        c.dedup();
        c
    }

    /// One atomic mutate: budget, campaign, location criteria, ad group,
    /// responsive search ad and keywords, linked by temporary negative ids.
    /// `geo_targets` are `geoTargetConstants/...` resource names.
    pub fn operations(
        &self,
        customer_id: &str,
        geo_targets: &[String],
    ) -> Result<Vec<Value>, String> {
        self.validate()?;
        let budget = format!("customers/{customer_id}/campaignBudgets/-1");
        let campaign = format!("customers/{customer_id}/campaigns/-2");
        let ad_group = format!("customers/{customer_id}/adGroups/-3");
        let name = self.name.trim();
        let mut ops = vec![json!({"campaignBudgetOperation": {"create": {
            "resourceName": budget,
            "name": format!("{name} budget"),
            "amountMicros": micros(money(&self.daily_budget, "daily_budget")?),
            "deliveryMethod": "STANDARD",
            "explicitlyShared": false
        }}})];

        let mut campaign_body = json!({
            "resourceName": campaign,
            "name": name,
            "advertisingChannelType": "SEARCH",
            "status": "PAUSED",
            "campaignBudget": budget,
            "containsEuPoliticalAdvertising": if self.contains_eu_political_advertising {
                "CONTAINS_EU_POLITICAL_ADVERTISING"
            } else {
                "DOES_NOT_CONTAIN_EU_POLITICAL_ADVERTISING"
            },
            "networkSettings": {
                "targetGoogleSearch": true,
                "targetSearchNetwork": true,
                "targetContentNetwork": false,
                "targetPartnerSearchNetwork": false
            }
        });
        let mut ad_group_body = json!({
            "resourceName": ad_group,
            "campaign": campaign,
            "name": self.ad_group_name.as_deref().map(str::trim).filter(|n| !n.is_empty()).unwrap_or(name),
            "status": "ENABLED",
            "type": "SEARCH_STANDARD"
        });
        match &self.max_cpc {
            Some(cpc) => {
                campaign_body["manualCpc"] = json!({});
                ad_group_body["cpcBidMicros"] = json!(micros(money(cpc, "max_cpc")?));
            }
            // Maximize clicks is `targetSpend` in the API.
            None => campaign_body["targetSpend"] = json!({}),
        }
        ops.push(json!({"campaignOperation": {"create": campaign_body}}));
        for geo in geo_targets {
            ops.push(json!({"campaignCriterionOperation": {"create": {
                "campaign": campaign,
                "location": {"geoTargetConstant": geo}
            }}}));
        }
        ops.push(json!({"adGroupOperation": {"create": ad_group_body}}));
        ops.push(json!({"adGroupAdOperation": {"create": {
            "adGroup": ad_group,
            "status": "ENABLED",
            "ad": {
                "finalUrls": [self.final_url.trim()],
                "responsiveSearchAd": {
                    "headlines": self.headlines.iter().map(|h| json!({"text": h.trim()})).collect::<Vec<_>>(),
                    "descriptions": self.descriptions.iter().map(|d| json!({"text": d.trim()})).collect::<Vec<_>>()
                }
            }
        }}}));
        let match_type = self.match_type().expect("validated");
        for k in &self.keywords {
            ops.push(json!({"adGroupCriterionOperation": {"create": {
                "adGroup": ad_group,
                "status": "ENABLED",
                "keyword": {"text": k.trim(), "matchType": match_type}
            }}}));
        }
        Ok(ops)
    }
}

/// The created campaign's id from a mutate response.
pub fn created_campaign_id(response: &Value) -> Option<String> {
    response["mutateOperationResponses"]
        .as_array()?
        .iter()
        .find_map(|r| r["campaignResult"]["resourceName"].as_str())
        .and_then(|name| name.rsplit('/').next())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn draft() -> CampaignDraft {
        serde_json::from_value(json!({
            "name": "Hoodies autumn",
            "daily_budget": "20",
            "final_url": "https://shop.test/hoodies",
            "headlines": ["Blue hoodies", "Free shipping", "Autumn sale"],
            "descriptions": ["Warm cotton hoodies in every size.", "Order today, delivered tomorrow."],
            "keywords": ["blue hoodie", "cotton hoodie"],
            "country_codes": ["es"],
            "contains_eu_political_advertising": false
        }))
        .unwrap()
    }

    #[test]
    fn builds_one_atomic_paused_search_campaign() {
        let ops = draft()
            .operations("1234567890", &["geoTargetConstants/2724".to_string()])
            .unwrap();
        assert_eq!(
            ops.len(),
            7,
            "budget, campaign, geo, ad group, ad, 2 keywords"
        );
        let budget = &ops[0]["campaignBudgetOperation"]["create"];
        assert_eq!(budget["amountMicros"], "20000000");
        assert_eq!(
            budget["resourceName"],
            "customers/1234567890/campaignBudgets/-1"
        );
        let campaign = &ops[1]["campaignOperation"]["create"];
        assert_eq!(campaign["status"], "PAUSED");
        assert_eq!(campaign["advertisingChannelType"], "SEARCH");
        assert_eq!(campaign["campaignBudget"], budget["resourceName"]);
        assert_eq!(
            campaign["containsEuPoliticalAdvertising"],
            "DOES_NOT_CONTAIN_EU_POLITICAL_ADVERTISING"
        );
        assert_eq!(
            campaign["targetSpend"],
            json!({}),
            "maximize clicks without a max CPC"
        );
        assert_eq!(
            ops[2]["campaignCriterionOperation"]["create"]["location"]["geoTargetConstant"],
            "geoTargetConstants/2724"
        );
        let ad = &ops[4]["adGroupAdOperation"]["create"]["ad"];
        assert_eq!(
            ad["responsiveSearchAd"]["headlines"][1]["text"],
            "Free shipping"
        );
        assert_eq!(ad["finalUrls"][0], "https://shop.test/hoodies");
        assert_eq!(
            ops[6]["adGroupCriterionOperation"]["create"]["keyword"],
            json!({"text": "cotton hoodie", "matchType": "PHRASE"})
        );
    }

    #[test]
    fn max_cpc_switches_to_manual_bidding_and_rounds_to_cents() {
        let mut d = draft();
        d.max_cpc = Some(json!(0.456));
        d.daily_budget = json!("12,5");
        let ops = d.operations("1234567890", &[]).unwrap();
        assert_eq!(
            ops[0]["campaignBudgetOperation"]["create"]["amountMicros"],
            "12500000"
        );
        assert_eq!(
            ops[1]["campaignOperation"]["create"]["manualCpc"],
            json!({})
        );
        assert_eq!(
            ops[2]["adGroupOperation"]["create"]["cpcBidMicros"],
            "460000"
        );
    }

    #[test]
    fn limits_are_checked_before_calling_google() {
        let mut d = draft();
        d.headlines = vec![
            "This headline is far too long to fit".into(),
            "Ok".into(),
            "ok".into(),
        ];
        d.descriptions = vec!["one".into()];
        d.final_url = "shop.test".into();
        d.match_type = Some("fuzzy".into());
        d.daily_budget = json!(0);
        let err = d.validate().unwrap_err();
        for expected in [
            "1-30 characters",
            "different",
            "2 to 4 descriptions",
            "final_url",
            "match_type",
            "greater than zero",
        ] {
            assert!(err.contains(expected), "missing {expected:?} in {err}");
        }
    }

    #[test]
    fn campaign_id_is_read_from_the_mutate_response() {
        let resp = json!({"mutateOperationResponses": [
            {"campaignBudgetResult": {"resourceName": "customers/1/campaignBudgets/55"}},
            {"campaignResult": {"resourceName": "customers/1/campaigns/987"}}
        ]});
        assert_eq!(created_campaign_id(&resp).as_deref(), Some("987"));
    }
}
