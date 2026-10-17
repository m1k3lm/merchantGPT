//! Typed queries per table.

use std::collections::BTreeMap;

use rusqlite::{params, OptionalExtension, Row};

use crate::{now, Result, Store, StoreError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShopRow {
    pub id: i64,
    pub url: String,
    pub name: Option<String>,
    pub created_at: String,
    pub last_used_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialsRow {
    pub kind: String,
    pub credentials: BTreeMap<String, String>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct LlmSettingsRow {
    pub provider: String,
    pub api_key: String,
    pub base_url: Option<String>,
    pub model: String,
    pub active: bool,
    /// Provider-specific values; may contain secrets (e.g. an AWS session token).
    pub options: BTreeMap<String, String>,
}

impl std::fmt::Debug for LlmSettingsRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmSettingsRow")
            .field("provider", &self.provider)
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("active", &self.active)
            .field("options", &self.options.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationRow {
    pub id: i64,
    pub shop_id: Option<i64>,
    pub title: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRow {
    pub id: i64,
    pub shop_id: Option<i64>,
    pub text: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTask {
    pub shop_id: i64,
    pub name: String,
    pub prompt: String,
    pub schedule: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub id: i64,
    pub shop_id: i64,
    pub name: String,
    pub prompt: String,
    pub schedule: String,
    pub enabled: bool,
    pub last_run_at: Option<String>,
    pub conversation_id: Option<i64>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAudit {
    pub shop_id: Option<i64>,
    pub conversation_id: Option<i64>,
    pub tool: String,
    pub arguments: String,
    pub stage: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRow {
    pub id: i64,
    pub shop_id: Option<i64>,
    pub conversation_id: Option<i64>,
    pub tool: String,
    pub arguments: String,
    pub stage: String,
    pub detail: Option<String>,
    pub created_at: String,
}

fn shop_row(r: &Row) -> rusqlite::Result<ShopRow> {
    Ok(ShopRow {
        id: r.get(0)?,
        url: r.get(1)?,
        name: r.get(2)?,
        created_at: r.get(3)?,
        last_used_at: r.get(4)?,
    })
}

fn conversation_row(r: &Row) -> rusqlite::Result<ConversationRow> {
    Ok(ConversationRow {
        id: r.get(0)?,
        shop_id: r.get(1)?,
        title: r.get(2)?,
        input_tokens: r.get::<_, i64>(3)? as u64,
        output_tokens: r.get::<_, i64>(4)? as u64,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
    })
}

fn task_row(r: &Row) -> rusqlite::Result<TaskRow> {
    Ok(TaskRow {
        id: r.get(0)?,
        shop_id: r.get(1)?,
        name: r.get(2)?,
        prompt: r.get(3)?,
        schedule: r.get(4)?,
        enabled: r.get(5)?,
        last_run_at: r.get(6)?,
        conversation_id: r.get(7)?,
        created_at: r.get(8)?,
    })
}

const SHOP_COLS: &str = "id, url, name, created_at, last_used_at";
const CONV_COLS: &str = "id, shop_id, title, input_tokens, output_tokens, created_at, updated_at";
const TASK_COLS: &str =
    "id, shop_id, name, prompt, schedule, enabled, last_run_at, conversation_id, created_at";

impl Store {
    // --- shops -----------------------------------------------------------

    /// Inserts the shop or returns the existing one with the same URL,
    /// marking it as most recently used.
    pub fn upsert_shop(&self, url: &str, name: Option<&str>) -> Result<ShopRow> {
        let conn = self.conn();
        let ts = now();
        conn.execute(
            "INSERT INTO shops (url, name, created_at, last_used_at) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(url) DO UPDATE SET last_used_at = ?3, name = COALESCE(?2, name)",
            params![url, name, ts],
        )?;
        Ok(conn.query_row(
            &format!("SELECT {SHOP_COLS} FROM shops WHERE url = ?1"),
            [url],
            shop_row,
        )?)
    }

    /// Most recently used first.
    pub fn list_shops(&self) -> Result<Vec<ShopRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {SHOP_COLS} FROM shops ORDER BY last_used_at DESC, id DESC"
        ))?;
        let rows = stmt
            .query_map([], shop_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn shop(&self, id: i64) -> Result<Option<ShopRow>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {SHOP_COLS} FROM shops WHERE id = ?1"),
                [id],
                shop_row,
            )
            .optional()?)
    }

    pub fn touch_shop(&self, id: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE shops SET last_used_at = ?2 WHERE id = ?1",
            params![id, now()],
        )?;
        Ok(())
    }

    pub fn delete_shop(&self, id: i64) -> Result<()> {
        self.conn()
            .execute("DELETE FROM shops WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn save_credentials(
        &self,
        shop_id: i64,
        kind: &str,
        credentials: &BTreeMap<String, String>,
    ) -> Result<()> {
        let json =
            serde_json::to_string(credentials).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        self.conn().execute(
            "INSERT INTO connector_credentials (shop_id, kind, credentials, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(shop_id, kind) DO UPDATE SET credentials = ?3, updated_at = ?4",
            params![shop_id, kind, json, now()],
        )?;
        Ok(())
    }

    pub fn credentials(&self, shop_id: i64) -> Result<Vec<CredentialsRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT kind, credentials FROM connector_credentials WHERE shop_id = ?1 ORDER BY kind",
        )?;
        let raw: Vec<(String, String)> = stmt
            .query_map([shop_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        raw.into_iter()
            .map(|(kind, json)| {
                let credentials = serde_json::from_str(&json)
                    .map_err(|e| StoreError::Corrupt(format!("credentials for {kind}: {e}")))?;
                Ok(CredentialsRow { kind, credentials })
            })
            .collect()
    }

    pub fn delete_credentials(&self, shop_id: i64, kind: &str) -> Result<bool> {
        Ok(self.conn().execute(
            "DELETE FROM connector_credentials WHERE shop_id = ?1 AND kind = ?2",
            params![shop_id, kind],
        )? > 0)
    }

    // --- app settings ------------------------------------------------------

    pub fn set_app_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = ?2, updated_at = ?3",
            params![key, value, now()],
        )?;
        Ok(())
    }

    pub fn app_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                [key],
                |r| r.get(0),
            )
            .optional()?)
    }

    // --- LLM settings ------------------------------------------------------

    /// Saves settings for one provider; if `active`, it becomes the only active one.
    pub fn save_llm_settings(&self, s: &LlmSettingsRow) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if s.active {
            tx.execute("UPDATE llm_settings SET active = 0", [])?;
        }
        let options =
            serde_json::to_string(&s.options).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        tx.execute(
            "INSERT INTO llm_settings (provider, api_key, base_url, model, active, updated_at, options)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(provider) DO UPDATE SET api_key = ?2, base_url = ?3, model = ?4,
                 active = ?5, updated_at = ?6, options = ?7",
            params![s.provider, s.api_key, s.base_url, s.model, s.active, now(), options],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn llm_settings(&self) -> Result<Vec<LlmSettingsRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT provider, api_key, base_url, model, active, options FROM llm_settings ORDER BY provider",
        )?;
        let raw: Vec<(LlmSettingsRow, String)> = stmt
            .query_map([], |r| {
                Ok((
                    LlmSettingsRow {
                        provider: r.get(0)?,
                        api_key: r.get(1)?,
                        base_url: r.get(2)?,
                        model: r.get(3)?,
                        active: r.get(4)?,
                        options: BTreeMap::new(),
                    },
                    r.get(5)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        raw.into_iter()
            .map(|(mut row, json)| {
                row.options = serde_json::from_str(&json).map_err(|e| {
                    StoreError::Corrupt(format!("options for {}: {e}", row.provider))
                })?;
                Ok(row)
            })
            .collect()
    }

    pub fn active_llm(&self) -> Result<Option<LlmSettingsRow>> {
        Ok(self.llm_settings()?.into_iter().find(|s| s.active))
    }

    // --- conversations -----------------------------------------------------

    pub fn create_conversation(&self, shop_id: Option<i64>, title: &str) -> Result<i64> {
        let conn = self.conn();
        let ts = now();
        conn.execute(
            "INSERT INTO conversations (shop_id, title, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
            params![shop_id, title, ts],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Conversations of one shop (or the shop-less ones), most recent first.
    pub fn list_conversations(&self, shop_id: Option<i64>) -> Result<Vec<ConversationRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {CONV_COLS} FROM conversations WHERE shop_id IS ?1
             ORDER BY updated_at DESC, id DESC"
        ))?;
        let rows = stmt
            .query_map([shop_id], conversation_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn conversation(&self, id: i64) -> Result<Option<ConversationRow>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {CONV_COLS} FROM conversations WHERE id = ?1"),
                [id],
                conversation_row,
            )
            .optional()?)
    }

    pub fn rename_conversation(&self, id: i64, title: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE conversations SET title = ?2 WHERE id = ?1",
            params![id, title],
        )?;
        Ok(())
    }

    pub fn delete_conversation(&self, id: i64) -> Result<()> {
        self.conn()
            .execute("DELETE FROM conversations WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Appends serialized messages after the existing ones.
    pub fn append_messages(&self, conversation_id: i64, messages: &[String]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let ts = now();
        let next: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM messages WHERE conversation_id = ?1",
            [conversation_id],
            |r| r.get(0),
        )?;
        for (i, m) in messages.iter().enumerate() {
            tx.execute(
                "INSERT INTO messages (conversation_id, seq, message, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![conversation_id, next + i as i64, m, ts],
            )?;
        }
        tx.execute(
            "UPDATE conversations SET updated_at = ?2 WHERE id = ?1",
            params![conversation_id, ts],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn messages(&self, conversation_id: i64) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT message FROM messages WHERE conversation_id = ?1 ORDER BY seq")?;
        let rows = stmt
            .query_map([conversation_id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn add_usage(&self, conversation_id: i64, input: u64, output: u64) -> Result<()> {
        self.conn().execute(
            "UPDATE conversations SET input_tokens = input_tokens + ?2,
                 output_tokens = output_tokens + ?3 WHERE id = ?1",
            params![conversation_id, input as i64, output as i64],
        )?;
        Ok(())
    }

    // --- memory ------------------------------------------------------------

    pub fn add_memory(&self, shop_id: Option<i64>, text: &str) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO memory_notes (shop_id, text, created_at) VALUES (?1, ?2, ?3)",
            params![shop_id, text, now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Global notes plus the shop's own, oldest first.
    pub fn memory_notes(&self, shop_id: Option<i64>) -> Result<Vec<MemoryRow>> {
        self.query_memory(shop_id, None)
    }

    /// Case-insensitive substring search over the notes visible to the shop.
    pub fn search_memory(&self, shop_id: Option<i64>, query: &str) -> Result<Vec<MemoryRow>> {
        self.query_memory(shop_id, Some(query))
    }

    fn query_memory(&self, shop_id: Option<i64>, query: Option<&str>) -> Result<Vec<MemoryRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, shop_id, text, created_at FROM memory_notes
             WHERE (shop_id IS NULL OR shop_id IS ?1)
               AND (?2 IS NULL OR text LIKE '%' || ?2 || '%' ESCAPE '\\')
             ORDER BY id",
        )?;
        let escaped = query.map(|q| {
            q.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        });
        let rows = stmt
            .query_map(params![shop_id, escaped], |r| {
                Ok(MemoryRow {
                    id: r.get(0)?,
                    shop_id: r.get(1)?,
                    text: r.get(2)?,
                    created_at: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn delete_memory(&self, id: i64) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM memory_notes WHERE id = ?1", [id])?
            > 0)
    }

    // --- scheduled tasks ---------------------------------------------------

    pub fn create_task(&self, t: &NewTask) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO scheduled_tasks (shop_id, name, prompt, schedule, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![t.shop_id, t.name, t.prompt, t.schedule, now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// All tasks, or only one shop's.
    pub fn list_tasks(&self, shop_id: Option<i64>) -> Result<Vec<TaskRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {TASK_COLS} FROM scheduled_tasks WHERE ?1 IS NULL OR shop_id = ?1 ORDER BY id"
        ))?;
        let rows = stmt
            .query_map([shop_id], task_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn set_task_enabled(&self, id: i64, enabled: bool) -> Result<()> {
        self.conn().execute(
            "UPDATE scheduled_tasks SET enabled = ?2 WHERE id = ?1",
            params![id, enabled],
        )?;
        Ok(())
    }

    pub fn delete_task(&self, id: i64) -> Result<()> {
        self.conn()
            .execute("DELETE FROM scheduled_tasks WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn mark_task_run(&self, id: i64, at: &str, conversation_id: Option<i64>) -> Result<()> {
        self.conn().execute(
            "UPDATE scheduled_tasks SET last_run_at = ?2,
                 conversation_id = COALESCE(?3, conversation_id) WHERE id = ?1",
            params![id, at, conversation_id],
        )?;
        Ok(())
    }

    // --- audit -------------------------------------------------------------

    pub fn record_audit(&self, a: &NewAudit) -> Result<()> {
        self.conn().execute(
            "INSERT INTO audit_log (shop_id, conversation_id, tool, arguments, stage, detail, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![a.shop_id, a.conversation_id, a.tool, a.arguments, a.stage, a.detail, now()],
        )?;
        Ok(())
    }

    /// Newest first.
    pub fn audit_log(&self, shop_id: Option<i64>, limit: u32) -> Result<Vec<AuditRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, shop_id, conversation_id, tool, arguments, stage, detail, created_at
             FROM audit_log WHERE ?1 IS NULL OR shop_id = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![shop_id, limit], |r| {
                Ok(AuditRow {
                    id: r.get(0)?,
                    shop_id: r.get(1)?,
                    conversation_id: r.get(2)?,
                    tool: r.get(3)?,
                    arguments: r.get(4)?,
                    stage: r.get(5)?,
                    detail: r.get(6)?,
                    created_at: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn shop_upsert_is_idempotent_and_credentials_roundtrip() {
        let s = store();
        let a = s.upsert_shop("https://a.test", Some("A")).unwrap();
        let again = s.upsert_shop("https://a.test", None).unwrap();
        assert_eq!(a.id, again.id);
        assert_eq!(again.name.as_deref(), Some("A"));
        let creds = BTreeMap::from([("consumer_key".to_string(), "ck".to_string())]);
        s.save_credentials(a.id, "shop:woocommerce", &creds)
            .unwrap();
        s.save_credentials(a.id, "shop:woocommerce", &creds)
            .unwrap();
        assert_eq!(
            s.credentials(a.id).unwrap(),
            vec![CredentialsRow {
                kind: "shop:woocommerce".into(),
                credentials: creds
            }]
        );
        s.delete_shop(a.id).unwrap();
        assert!(s.credentials(a.id).unwrap().is_empty(), "cascade");
    }

    #[test]
    fn app_settings_and_single_connector_removal() {
        let s = store();
        assert_eq!(s.app_setting("k").unwrap(), None);
        s.set_app_setting("k", "v1").unwrap();
        s.set_app_setting("k", "v2").unwrap();
        assert_eq!(s.app_setting("k").unwrap().as_deref(), Some("v2"));

        let shop = s.upsert_shop("https://a.test", None).unwrap().id;
        s.save_credentials(shop, "shop:woocommerce", &BTreeMap::new())
            .unwrap();
        s.save_credentials(shop, "ads:google", &BTreeMap::new())
            .unwrap();
        assert!(s.delete_credentials(shop, "ads:google").unwrap());
        assert!(!s.delete_credentials(shop, "ads:google").unwrap());
        assert_eq!(s.credentials(shop).unwrap().len(), 1);
    }

    #[test]
    fn only_one_llm_setting_is_active() {
        let s = store();
        let row = |p: &str, active| LlmSettingsRow {
            options: BTreeMap::from([("region".to_string(), "eu-west-1".to_string())]),
            provider: p.into(),
            api_key: "k".into(),
            base_url: None,
            model: "m".into(),
            active,
        };
        s.save_llm_settings(&row("openai", true)).unwrap();
        s.save_llm_settings(&row("anthropic", true)).unwrap();
        assert_eq!(s.active_llm().unwrap().unwrap().provider, "anthropic");
        assert_eq!(
            s.active_llm().unwrap().unwrap().options["region"],
            "eu-west-1"
        );
        assert_eq!(s.llm_settings().unwrap().len(), 2);
        assert!(!format!("{:?}", row("x", true)).contains("\"k\""));
    }

    #[test]
    fn conversation_roundtrip() {
        let s = store();
        let shop = s.upsert_shop("https://a.test", None).unwrap();
        let c = s.create_conversation(Some(shop.id), "Orders").unwrap();
        s.append_messages(c, &["m0".into(), "m1".into()]).unwrap();
        s.append_messages(c, &["m2".into()]).unwrap();
        s.add_usage(c, 10, 5).unwrap();
        assert_eq!(s.messages(c).unwrap(), ["m0", "m1", "m2"]);
        let row = s.conversation(c).unwrap().unwrap();
        assert_eq!((row.input_tokens, row.output_tokens), (10, 5));
        assert_eq!(s.list_conversations(Some(shop.id)).unwrap().len(), 1);
        assert!(s.list_conversations(None).unwrap().is_empty());
        s.rename_conversation(c, "Renamed").unwrap();
        assert_eq!(s.conversation(c).unwrap().unwrap().title, "Renamed");
        s.delete_conversation(c).unwrap();
        assert!(s.messages(c).unwrap().is_empty());
    }

    #[test]
    fn memory_is_scoped_to_shop_plus_global_and_search_escapes_wildcards() {
        let s = store();
        let a = s.upsert_shop("https://a.test", None).unwrap().id;
        let b = s.upsert_shop("https://b.test", None).unwrap().id;
        s.add_memory(None, "Merchant prefers EUR").unwrap();
        s.add_memory(Some(a), "A ships on Mondays").unwrap();
        s.add_memory(Some(b), "B has 100% markup").unwrap();
        let texts = |rows: Vec<MemoryRow>| rows.into_iter().map(|r| r.text).collect::<Vec<_>>();
        assert_eq!(
            texts(s.memory_notes(Some(a)).unwrap()),
            ["Merchant prefers EUR", "A ships on Mondays"]
        );
        assert_eq!(
            texts(s.search_memory(Some(b), "100%").unwrap()),
            ["B has 100% markup"]
        );
        assert!(s.search_memory(Some(a), "%").unwrap().is_empty());
        assert_eq!(
            texts(s.search_memory(Some(a), "eur").unwrap()),
            ["Merchant prefers EUR"]
        );
    }

    #[test]
    fn tasks_and_audit_roundtrip() {
        let s = store();
        let shop = s.upsert_shop("https://a.test", None).unwrap().id;
        let t = s
            .create_task(&NewTask {
                shop_id: shop,
                name: "Daily sales".into(),
                prompt: "Summarise yesterday".into(),
                schedule: "daily 09:00".into(),
            })
            .unwrap();
        s.mark_task_run(t, "2026-10-17T09:00:00Z", None).unwrap();
        s.set_task_enabled(t, false).unwrap();
        let row = &s.list_tasks(Some(shop)).unwrap()[0];
        assert_eq!(row.last_run_at.as_deref(), Some("2026-10-17T09:00:00Z"));
        assert!(!row.enabled);
        assert_eq!(s.list_tasks(None).unwrap().len(), 1);

        s.record_audit(&NewAudit {
            shop_id: Some(shop),
            conversation_id: None,
            tool: "shop_update_product".into(),
            arguments: r#"{"id":"1"}"#.into(),
            stage: "approved".into(),
            detail: None,
        })
        .unwrap();
        let log = s.audit_log(Some(shop), 10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].stage, "approved");
    }
}
