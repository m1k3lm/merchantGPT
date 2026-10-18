//! The webservice reads JSON but only accepts XML bodies. Writes here are
//! small partial documents, so a writer with escaping is all that is needed.

pub(crate) enum Field {
    Text(String),
    /// A multilang field. PrestaShop blanks any language left out, so every
    /// language is sent.
    Lang(Vec<(u64, String)>),
    /// Already-built XML, such as `associations`.
    Xml(String),
}

pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// `<prestashop><{resource}>{fields}</{resource}></prestashop>`
pub(crate) fn document(resource: &str, fields: &[(&str, Field)]) -> String {
    let mut body = String::new();
    for (name, value) in fields {
        match value {
            Field::Text(v) => body.push_str(&format!("<{name}>{}</{name}>", escape(v))),
            Field::Xml(inner) => body.push_str(&format!("<{name}>{inner}</{name}>")),
            Field::Lang(values) => {
                body.push_str(&format!("<{name}>"));
                for (id, v) in values {
                    body.push_str(&format!("<language id=\"{id}\">{}</language>", escape(v)));
                }
                body.push_str(&format!("</{name}>"));
            }
        }
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><prestashop xmlns:xlink=\"http://www.w3.org/1999/xlink\"><{resource}>{body}</{resource}></prestashop>"
    )
}

/// `<{list}><{item}><k>v</k>...</{item}>...</{list}>`, e.g. the categories
/// or cart rows inside `associations`.
pub(crate) fn rows(list: &str, item: &str, rows: &[Vec<(&str, String)>]) -> String {
    let mut out = format!("<{list}>");
    for row in rows {
        out.push_str(&format!("<{item}>"));
        for (k, v) in row {
            out.push_str(&format!("<{k}>{}</{k}>", escape(v)));
        }
        out.push_str(&format!("</{item}>"));
    }
    out.push_str(&format!("</{list}>"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn associations_nest_rows() {
        let doc = document(
            "product",
            &[(
                "associations",
                Field::Xml(rows(
                    "categories",
                    "category",
                    &[vec![("id", "2".into())], vec![("id", "10".into())]],
                )),
            )],
        );
        assert!(doc.contains("<associations><categories><category><id>2</id></category><category><id>10</id></category></categories></associations>"));
    }

    #[test]
    fn documents_escape_values_and_nest_languages() {
        let doc = document(
            "product",
            &[
                ("id", Field::Text("7".into())),
                (
                    "name",
                    Field::Lang(vec![(1, "Tom & Jerry <mug>".into()), (2, "Taza".into())]),
                ),
            ],
        );
        assert!(doc.ends_with(
            "<product><id>7</id><name><language id=\"1\">Tom &amp; Jerry &lt;mug&gt;</language><language id=\"2\">Taza</language></name></product></prestashop>"
        ));
    }
}
