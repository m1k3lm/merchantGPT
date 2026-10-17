//! The agent's Google Ads tools.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{json, Value};
use tools::{parse_args, Mutability, Tool, ToolContext, ToolDefinition, ToolError};

use super::campaigns::{created_campaign_id, CampaignDraft};
use super::AdsContext;

#[derive(Clone, Copy)]
enum Op {
    ListCampaigns,
    CheckDraft,
    Draft,
}

pub(crate) fn ads_tools(ctx: Arc<AdsContext>) -> Vec<Arc<dyn Tool>> {
    [Op::ListCampaigns, Op::CheckDraft, Op::Draft]
        .into_iter()
        .map(|op| {
            Arc::new(AdsTool {
                ctx: ctx.clone(),
                op,
            }) as Arc<dyn Tool>
        })
        .collect()
}

struct AdsTool {
    ctx: Arc<AdsContext>,
    op: Op,
}

fn draft_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string", "description": "Campaign name, unique in the account"},
            "daily_budget": {"type": "string", "description": "Average daily budget in the account currency, e.g. \"20.00\""},
            "final_url": {"type": "string", "description": "Shop page the ad links to (full https URL)"},
            "headlines": {"type": "array", "items": {"type": "string"}, "description": "3-15 distinct headlines, each at most 30 characters"},
            "descriptions": {"type": "array", "items": {"type": "string"}, "description": "2-4 descriptions, each at most 90 characters"},
            "keywords": {"type": "array", "items": {"type": "string"}, "description": "Keywords to bid on"},
            "match_type": {"type": "string", "enum": ["BROAD", "PHRASE", "EXACT"], "description": "Keyword match type (default PHRASE)"},
            "max_cpc": {"type": "string", "description": "Max cost per click; omit to let Google maximize clicks within budget"},
            "country_codes": {"type": "array", "items": {"type": "string"}, "description": "Countries to target as ISO codes, e.g. [\"ES\"]; omit for all"},
            "contains_eu_political_advertising": {"type": "boolean", "description": "Legal declaration required by Google. Ask the merchant if unsure; product ads are normally false"},
            "ad_group_name": {"type": "string"}
        },
        "required": ["name", "daily_budget", "final_url", "headlines", "descriptions", "keywords", "contains_eu_political_advertising"]
    })
}

#[derive(Deserialize)]
struct ListArgs {
    #[serde(default)]
    days: Option<u32>,
}

fn failed<E: std::fmt::Display>(e: E) -> ToolError {
    ToolError::Failed(e.to_string())
}

/// Micros (as JSON string or number) to a currency amount.
fn from_micros(v: &Value) -> Option<String> {
    let raw = v
        .as_str()
        .map(str::to_string)
        .or_else(|| v.as_i64().map(|n| n.to_string()))?;
    let d = Decimal::from_str(&raw).ok()? / Decimal::from(1_000_000);
    Some(d.round_dp(2).normalize().to_string())
}

impl AdsTool {
    async fn geo_targets(&self, draft: &CampaignDraft) -> Result<Vec<String>, ToolError> {
        let codes = draft.country_codes();
        if codes.is_empty() {
            return Ok(Vec::new());
        }
        // Codes were validated as two ASCII letters, so quoting them is safe.
        let list = codes
            .iter()
            .map(|c| format!("'{c}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let rows = self
            .ctx
            .client
            .search(
                self.ctx.login_customer_id.as_deref(),
                &self.ctx.customer_id,
                &format!("SELECT geo_target_constant.resource_name, geo_target_constant.country_code FROM geo_target_constant WHERE geo_target_constant.country_code IN ({list}) AND geo_target_constant.target_type = 'Country' AND geo_target_constant.status = 'ENABLED'"),
            )
            .await
            .map_err(failed)?;
        let found: BTreeMap<String, String> = rows
            .iter()
            .filter_map(|r| {
                let g = &r["geoTargetConstant"];
                Some((
                    g["countryCode"].as_str()?.to_string(),
                    g["resourceName"].as_str()?.to_string(),
                ))
            })
            .collect();
        codes
            .iter()
            .map(|c| {
                found
                    .get(c)
                    .cloned()
                    .ok_or_else(|| ToolError::InvalidArguments(format!("unknown country code {c}")))
            })
            .collect()
    }

    async fn mutate_draft(&self, args: Value, validate_only: bool) -> Result<Value, ToolError> {
        let draft: CampaignDraft = parse_args(args)?;
        draft.validate().map_err(ToolError::InvalidArguments)?;
        let geo = self.geo_targets(&draft).await?;
        let ops = draft
            .operations(&self.ctx.customer_id, &geo)
            .map_err(ToolError::InvalidArguments)?;
        self.ctx
            .client
            .mutate(
                self.ctx.login_customer_id.as_deref(),
                &self.ctx.customer_id,
                ops,
                validate_only,
            )
            .await
            .map_err(failed)
    }

    async fn list_campaigns(&self, args: Value) -> Result<String, ToolError> {
        let a: ListArgs = parse_args(args)?;
        let range = match a.days.unwrap_or(30) {
            0..=7 => "LAST_7_DAYS",
            8..=14 => "LAST_14_DAYS",
            _ => "LAST_30_DAYS",
        };
        let (login, customer) = (self.ctx.login_customer_id.as_deref(), &self.ctx.customer_id);
        // Two queries: the first lists every campaign, including paused drafts
        // without activity, which a metrics query would leave out.
        let campaigns = self
            .ctx
            .client
            .search(login, customer, "SELECT campaign.id, campaign.name, campaign.status, campaign.advertising_channel_type, campaign_budget.amount_micros FROM campaign WHERE campaign.status != 'REMOVED' ORDER BY campaign.id DESC LIMIT 200")
            .await
            .map_err(failed)?;
        let metrics = self
            .ctx
            .client
            .search(login, customer, &format!("SELECT campaign.id, metrics.impressions, metrics.clicks, metrics.cost_micros, metrics.conversions FROM campaign WHERE campaign.status != 'REMOVED' AND segments.date DURING {range}"))
            .await
            .map_err(failed)?;
        let by_id: BTreeMap<String, &Value> = metrics
            .iter()
            .filter_map(|r| Some((r["campaign"]["id"].as_str()?.to_string(), &r["metrics"])))
            .collect();
        let out: Vec<Value> = campaigns
            .iter()
            .map(|r| {
                let id = r["campaign"]["id"].as_str().unwrap_or_default();
                let m = by_id.get(id).copied().unwrap_or(&Value::Null);
                json!({
                    "id": id,
                    "name": r["campaign"]["name"],
                    "status": r["campaign"]["status"],
                    "type": r["campaign"]["advertisingChannelType"],
                    "daily_budget": from_micros(&r["campaignBudget"]["amountMicros"]),
                    "impressions": m["impressions"].as_str().unwrap_or("0"),
                    "clicks": m["clicks"].as_str().unwrap_or("0"),
                    "cost": from_micros(&m["costMicros"]).unwrap_or_else(|| "0".into()),
                    "conversions": m["conversions"].as_f64().unwrap_or(0.0),
                })
            })
            .collect();
        Ok(json!({"period": range, "campaigns": out}).to_string())
    }
}

#[async_trait]
impl Tool for AdsTool {
    fn definition(&self) -> ToolDefinition {
        match self.op {
            Op::ListCampaigns => ToolDefinition {
                name: "ads_list_campaigns".into(),
                description: "List the shop's Google Ads campaigns with status, daily budget and recent impressions, clicks, cost and conversions.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"days": {"type": "integer", "description": "Metrics period: 7, 14 or 30 days (default 30)"}}
                }),
                mutability: Mutability::Read,
            },
            Op::CheckDraft => ToolDefinition {
                name: "ads_check_campaign_draft".into(),
                description: "Ask Google Ads to validate a Search campaign draft without creating anything. Use it before ads_draft_search_campaign to catch policy or format problems.".into(),
                parameters: draft_schema(),
                mutability: Mutability::Read,
            },
            Op::Draft => ToolDefinition {
                name: "ads_draft_search_campaign".into(),
                description: "Create a Search campaign draft in Google Ads: budget, campaign, ad group, responsive search ad and keywords in one step. It is created PAUSED, so it does not run or spend until the merchant enables it in Google Ads.".into(),
                parameters: draft_schema(),
                mutability: Mutability::Mutate,
            },
        }
    }

    async fn call(&self, args: Value, _: &ToolContext) -> Result<String, ToolError> {
        match self.op {
            Op::ListCampaigns => self.list_campaigns(args).await,
            Op::CheckDraft => {
                self.mutate_draft(args, true).await?;
                Ok(json!({"valid": true, "note": "Google accepted the draft; nothing was created."}).to_string())
            }
            Op::Draft => {
                let resp = self.mutate_draft(args, false).await?;
                Ok(json!({
                    "created": true,
                    "campaign_id": created_campaign_id(&resp),
                    "status": "PAUSED",
                    "note": "The campaign is paused. Review it in Google Ads > Campaigns and enable it there when ready."
                })
                .to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::google_ads::{tests::creds, GoogleAds};
    use crate::transport::{FakeTransport, HttpResponse, Method};
    use crate::Connector;

    fn setup() -> (Arc<FakeTransport>, Vec<Arc<dyn Tool>>) {
        let t = Arc::new(FakeTransport::new());
        t.on(
            Method::Post,
            "/token",
            HttpResponse::new(200, r#"{"access_token":"a","expires_in":3600}"#),
        );
        let ads = GoogleAds::from_credentials(&creds(), t.clone()).unwrap();
        (t, ads.tools())
    }

    fn find(tools: &[Arc<dyn Tool>], name: &str) -> Arc<dyn Tool> {
        tools
            .iter()
            .find(|t| t.definition().name == name)
            .cloned()
            .unwrap()
    }

    fn draft_args() -> Value {
        json!({
            "name": "Hoodies", "daily_budget": "15", "final_url": "https://shop.test/h",
            "headlines": ["Blue hoodies", "Free shipping", "Autumn sale"],
            "descriptions": ["Warm cotton hoodies.", "Order today."],
            "keywords": ["blue hoodie"], "country_codes": ["ES"],
            "contains_eu_political_advertising": false
        })
    }

    fn geo(t: &FakeTransport) {
        t.on(
            Method::Post,
            "/v26/customers/1234567890/googleAds:search",
            HttpResponse::new(200, r#"{"results":[{"geoTargetConstant":{"resourceName":"geoTargetConstants/2724","countryCode":"ES"}}]}"#),
        );
    }

    #[tokio::test]
    async fn drafting_creates_a_paused_campaign_and_only_drafting_mutates() {
        let (t, tools) = setup();
        geo(&t);
        t.on(
            Method::Post,
            "/v26/customers/1234567890/googleAds:mutate",
            HttpResponse::new(200, r#"{"mutateOperationResponses":[{"campaignBudgetResult":{"resourceName":"customers/1234567890/campaignBudgets/5"}},{"campaignResult":{"resourceName":"customers/1234567890/campaigns/987"}}]}"#),
        );
        let draft = find(&tools, "ads_draft_search_campaign");
        assert_eq!(draft.definition().mutability, Mutability::Mutate);
        assert_eq!(
            find(&tools, "ads_check_campaign_draft")
                .definition()
                .mutability,
            Mutability::Read
        );

        let out: Value = serde_json::from_str(
            &draft
                .call(draft_args(), &ToolContext::default())
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["campaign_id"], "987");
        assert_eq!(out["status"], "PAUSED");
        let mutate = t
            .requests()
            .into_iter()
            .find(|r| r.path().ends_with(":mutate"))
            .unwrap();
        let body: Value = serde_json::from_str(mutate.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["validateOnly"], false);
        assert_eq!(
            body["mutateOperations"][1]["campaignOperation"]["create"]["status"],
            "PAUSED"
        );
        assert_eq!(
            body["mutateOperations"][2]["campaignCriterionOperation"]["create"]["location"]
                ["geoTargetConstant"],
            "geoTargetConstants/2724"
        );
    }

    #[tokio::test]
    async fn checking_a_draft_is_validate_only() {
        let (t, tools) = setup();
        geo(&t);
        t.on(
            Method::Post,
            "/v26/customers/1234567890/googleAds:mutate",
            HttpResponse::new(200, "{}"),
        );
        find(&tools, "ads_check_campaign_draft")
            .call(draft_args(), &ToolContext::default())
            .await
            .unwrap();
        let mutate = t
            .requests()
            .into_iter()
            .find(|r| r.path().ends_with(":mutate"))
            .unwrap();
        assert!(mutate
            .body
            .as_deref()
            .unwrap()
            .contains("\"validateOnly\":true"));
    }

    #[tokio::test]
    async fn invalid_drafts_never_reach_google() {
        let (t, tools) = setup();
        let mut args = draft_args();
        args["headlines"] = json!(["only one"]);
        let err = find(&tools, "ads_draft_search_campaign")
            .call(args, &ToolContext::default())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(m) if m.contains("3 to 15 headlines")));
        let mut args = draft_args();
        args.as_object_mut()
            .unwrap()
            .remove("contains_eu_political_advertising");
        assert!(find(&tools, "ads_draft_search_campaign")
            .call(args, &ToolContext::default())
            .await
            .is_err());
        assert!(t.requests().is_empty());
    }

    #[tokio::test]
    async fn campaigns_list_merges_paused_drafts_with_metrics() {
        let (t, tools) = setup();
        t.on(
            Method::Post,
            "/v26/customers/1234567890/googleAds:search",
            HttpResponse::new(200, r#"{"results":[
                {"campaign":{"id":"987","name":"Hoodies","status":"PAUSED","advertisingChannelType":"SEARCH"},"campaignBudget":{"amountMicros":"15000000"}},
                {"campaign":{"id":"5","name":"Brand","status":"ENABLED","advertisingChannelType":"SEARCH"},"campaignBudget":{"amountMicros":"7500000"}}]}"#),
        )
        .on(
            Method::Post,
            "/v26/customers/1234567890/googleAds:search",
            HttpResponse::new(200, r#"{"results":[{"campaign":{"id":"5"},"metrics":{"impressions":"1200","clicks":"80","costMicros":"23450000","conversions":3.0}}]}"#),
        );
        let out: Value = serde_json::from_str(
            &find(&tools, "ads_list_campaigns")
                .call(json!({"days": 7}), &ToolContext::default())
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["period"], "LAST_7_DAYS");
        assert_eq!(out["campaigns"][0]["daily_budget"], "15");
        assert_eq!(
            out["campaigns"][0]["clicks"], "0",
            "paused draft without activity is listed"
        );
        assert_eq!(out["campaigns"][1]["cost"], "23.45");
        assert_eq!(out["campaigns"][1]["conversions"], 3.0);
    }
}
