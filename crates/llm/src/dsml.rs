//! DeepSeek V4 sometimes writes its tool calls into the reply text as DSML
//! markup instead of returning `tool_calls`, often with degraded delimiters
//! (`< ｜｜DSML｜｜ invoke name="x">` instead of `<｜DSML｜invoke name="x">`).
//! `DsmlFilter` holds that markup back from the streamed text and turns it into
//! tool calls when the reply ends.

use serde_json::Value;

use crate::ToolCall;

enum Marker {
    No,
    /// The text ends while it could still become a marker.
    Partial,
    Full,
}

/// Matches `<`, optional spaces and `/`, one or more `｜` or `|`, optional
/// spaces, `DSML`, starting at byte `i` (a `<`).
fn marker_at(s: &str, i: usize) -> (Marker, usize) {
    let mut chars = s[i..].char_indices().skip(1).peekable();
    let mut pipes = 0;
    while let Some(&(_, c)) = chars.peek() {
        match c {
            ' ' | '/' if pipes == 0 => {}
            '｜' | '|' => pipes += 1,
            ' ' if pipes > 0 => {}
            _ => break,
        }
        chars.next();
    }
    let Some(&(at, _)) = chars.peek() else {
        return (Marker::Partial, 0);
    };
    if pipes == 0 {
        return (Marker::No, 0);
    }
    let rest = &s[i + at..];
    if rest.starts_with("DSML") {
        (Marker::Full, at + "DSML".len())
    } else if "DSML".starts_with(rest) {
        (Marker::Partial, 0)
    } else {
        (Marker::No, 0)
    }
}

#[derive(Default)]
pub(crate) struct DsmlFilter {
    pending: String,
    captured: Option<String>,
}

impl DsmlFilter {
    /// Returns the part of `text` that is safe to show now.
    pub fn push(&mut self, text: &str) -> String {
        if let Some(captured) = &mut self.captured {
            captured.push_str(text);
            return String::new();
        }
        self.pending.push_str(text);
        let mut from = 0;
        while let Some(off) = self.pending[from..].find('<') {
            let i = from + off;
            match marker_at(&self.pending, i).0 {
                Marker::Full => {
                    self.captured = Some(self.pending.split_off(i));
                    return std::mem::take(&mut self.pending);
                }
                Marker::Partial => {
                    let tail = self.pending.split_off(i);
                    return std::mem::replace(&mut self.pending, tail);
                }
                Marker::No => from = i + 1,
            }
        }
        std::mem::take(&mut self.pending)
    }

    /// Text still held back and the calls found in the markup. With
    /// `discard` (the reply already has real tool calls) the markup is
    /// dropped, so no call runs twice.
    pub fn finish(&mut self, discard: bool) -> (String, Vec<ToolCall>) {
        let pending = std::mem::take(&mut self.pending);
        let Some(captured) = self.captured.take() else {
            return (pending, Vec::new());
        };
        if discard {
            return (pending, Vec::new());
        }
        let calls = parse(&captured);
        if calls.is_empty() {
            (pending + &captured, calls)
        } else {
            (pending, calls)
        }
    }
}

/// Rewrites every DSML delimiter to a plain `<` or `</`.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(off) = s[i..].find('<') {
        let at = i + off;
        out.push_str(&s[i..at]);
        match marker_at(s, at) {
            (Marker::Full, len) => {
                let head = &s[at..at + len];
                out.push_str(if head.contains('/') { "</" } else { "<" });
                i = at + len;
                while let Some(c) = s[i..]
                    .chars()
                    .next()
                    .filter(|c| matches!(c, '｜' | '|' | ' '))
                {
                    i += c.len_utf8();
                }
            }
            _ => {
                out.push('<');
                i = at + 1;
            }
        }
    }
    out.push_str(&s[i..]);
    out
}

fn attr<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    let start = head.find(&format!("{name}=\""))? + name.len() + 2;
    let len = head[start..].find('"')?;
    Some(&head[start..start + len])
}

pub(crate) fn parse(text: &str) -> Vec<ToolCall> {
    let text = normalize(text);
    let mut calls = Vec::new();
    let mut rest = text.as_str();
    while let Some(start) = rest.find("<invoke") {
        rest = &rest[start..];
        let Some(head_end) = rest.find('>') else {
            break;
        };
        let name = attr(&rest[..head_end], "name").unwrap_or_default().trim();
        let body_end = rest.find("</invoke>").unwrap_or(rest.len());
        let body = &rest[head_end + 1..body_end.max(head_end + 1)];
        rest = &rest[body_end..];
        if name.is_empty() {
            continue;
        }
        let mut args = serde_json::Map::new();
        let mut params = body;
        while let Some(p) = params.find("<parameter") {
            params = &params[p..];
            let Some(head_end) = params.find('>') else {
                break;
            };
            let head = &params[..head_end];
            let value_end = params.find("</parameter>").unwrap_or(params.len());
            let raw = &params[head_end + 1..value_end.max(head_end + 1)];
            params = &params[value_end..];
            let Some(key) = attr(head, "name") else {
                continue;
            };
            let value = if attr(head, "string") == Some("true") {
                Value::String(raw.to_string())
            } else {
                serde_json::from_str(raw.trim()).unwrap_or_else(|_| Value::String(raw.to_string()))
            };
            args.insert(key.to_string(), value);
        }
        calls.push(ToolCall {
            id: format!("call_{}", uuid::Uuid::new_v4().simple()),
            name: name.to_string(),
            arguments: Value::Object(args),
        });
    }
    calls
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// What the app received from DeepSeek, verbatim.
    const DEGRADED: &str = r#"< ｜｜DSML｜｜ calls> < ｜｜DSML｜｜ invoke name="shop_api_write"> < ｜｜DSML｜｜ parameter name="method" string="true">POST</｜｜DSML｜｜ parameter> < ｜｜DSML｜｜ parameter name="path" string="true">/V1/products/attributes/puntas/options</｜｜DSML｜｜ parameter> < ｜｜DSML｜｜ parameter name="query" string="false">{"option[ is_default]": "0", "option[ label]": "4 puntas", "option[ sort_order]": "1"}</｜｜DSML｜｜ parameter> </｜｜DSML｜｜ invoke> < ｜｜DSML｜｜ invoke name="shop_api_write"> < ｜｜DSML｜｜ parameter name="method" string="true">POST</｜｜DSML｜｜ parameter> < ｜｜DSML｜｜ parameter name="path" string="true">/V1/products/attributes/puntas/options</｜｜DSML｜｜ parameter> < ｜｜DSML｜｜ parameter name="query" string="false">{"option[ label]": "5 puntas"}</｜｜DSML｜｜ parameter> </｜｜DSML｜｜ invoke> </｜｜DSML｜｜ calls>"#;

    const DOCUMENTED: &str = "<｜DSML｜function_calls>\n<｜DSML｜invoke name=\"shop_list_orders\">\n<｜DSML｜parameter name=\"status\" string=\"true\">processing</｜DSML｜parameter>\n<｜DSML｜parameter name=\"per_page\" string=\"false\">5</｜DSML｜parameter>\n</｜DSML｜invoke>\n</｜DSML｜function_calls>";

    fn stream(chunks: &[&str]) -> (String, Vec<ToolCall>) {
        let mut f = DsmlFilter::default();
        let mut shown: String = chunks.iter().map(|c| f.push(c)).collect();
        let (rest, calls) = f.finish(false);
        shown.push_str(&rest);
        (shown, calls)
    }

    /// Splits at every character boundary into chunks of `n` characters.
    fn chunked(s: &str, n: usize) -> Vec<String> {
        let chars: Vec<char> = s.chars().collect();
        chars.chunks(n).map(|c| c.iter().collect()).collect()
    }

    #[test]
    fn degraded_markup_becomes_calls_and_is_never_shown() {
        for n in [1, 3, 7, 1000] {
            let text = format!("Ahora añado las opciones. {DEGRADED}");
            let parts = chunked(&text, n);
            let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
            let (shown, calls) = stream(&refs);
            assert_eq!(shown, "Ahora añado las opciones. ", "chunk size {n}");
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].name, "shop_api_write");
            assert_eq!(
                calls[0].arguments,
                json!({
                    "method": "POST",
                    "path": "/V1/products/attributes/puntas/options",
                    "query": {"option[ is_default]": "0", "option[ label]": "4 puntas", "option[ sort_order]": "1"}
                })
            );
            assert_eq!(
                calls[1].arguments["query"],
                json!({"option[ label]": "5 puntas"})
            );
            assert_ne!(calls[0].id, calls[1].id);
        }
    }

    #[test]
    fn documented_markup_keeps_strings_and_parses_json_values() {
        let (shown, calls) = stream(&[DOCUMENTED]);
        assert_eq!(shown, "");
        assert_eq!(calls[0].name, "shop_list_orders");
        assert_eq!(
            calls[0].arguments,
            json!({"status": "processing", "per_page": 5})
        );
    }

    #[test]
    fn ordinary_angle_brackets_and_pipes_stay_text() {
        let text = "Si x < 3 y a <b> | c, total <| 4 ｜ precio < ｜ 10";
        assert_eq!(
            stream(
                &chunked(text, 2)
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
            ),
            (text.to_string(), Vec::new())
        );
        // A reply that ends inside what could have been a marker shows it.
        assert_eq!(stream(&["fin <｜DS"]).0, "fin <｜DS");
    }

    #[test]
    fn unparseable_markup_is_shown_and_markup_beside_real_calls_is_dropped() {
        let broken = "Hecho. <｜DSML｜function_calls> sin invocaciones";
        assert_eq!(stream(&[broken]), (broken.to_string(), Vec::new()));

        let mut f = DsmlFilter::default();
        assert_eq!(f.push(DOCUMENTED), "");
        assert_eq!(f.finish(true), (String::new(), Vec::new()));
    }
}
