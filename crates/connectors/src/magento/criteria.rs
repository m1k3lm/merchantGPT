//! Magento's `searchCriteria` query parameters. Filters inside a group are
//! ORed; groups are ANDed.

#[derive(Debug, Clone)]
pub(crate) struct Filter {
    pub field: &'static str,
    pub value: String,
    pub condition: &'static str,
}

impl Filter {
    pub fn eq(field: &'static str, value: impl Into<String>) -> Self {
        Self {
            field,
            value: value.into(),
            condition: "eq",
        }
    }

    pub fn like(field: &'static str, term: &str) -> Self {
        Self {
            field,
            value: format!("%{}%", term.trim()),
            condition: "like",
        }
    }

    pub fn cond(field: &'static str, condition: &'static str, value: impl Into<String>) -> Self {
        Self {
            field,
            value: value.into(),
            condition,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Criteria {
    pub groups: Vec<Vec<Filter>>,
    pub sort: Option<(&'static str, &'static str)>,
    pub page: u32,
    pub page_size: u32,
}

impl Criteria {
    pub fn page(page: u32, page_size: u32) -> Self {
        Self {
            page,
            page_size,
            ..Default::default()
        }
    }

    pub fn and(mut self, any_of: Vec<Filter>) -> Self {
        if !any_of.is_empty() {
            self.groups.push(any_of);
        }
        self
    }

    pub fn sort(mut self, field: &'static str, direction: &'static str) -> Self {
        self.sort = Some((field, direction));
        self
    }

    pub fn params(&self) -> Vec<(String, String)> {
        let mut p = Vec::new();
        for (g, filters) in self.groups.iter().enumerate() {
            for (i, f) in filters.iter().enumerate() {
                let base = format!("searchCriteria[filter_groups][{g}][filters][{i}]");
                p.push((format!("{base}[field]"), f.field.to_string()));
                p.push((format!("{base}[value]"), f.value.clone()));
                p.push((format!("{base}[condition_type]"), f.condition.to_string()));
            }
        }
        if let Some((field, dir)) = self.sort {
            p.push(("searchCriteria[sortOrders][0][field]".into(), field.into()));
            p.push((
                "searchCriteria[sortOrders][0][direction]".into(),
                dir.into(),
            ));
        }
        p.push((
            "searchCriteria[pageSize]".into(),
            self.page_size.to_string(),
        ));
        p.push(("searchCriteria[currentPage]".into(), self.page.to_string()));
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_and_filters_are_indexed() {
        let p = Criteria::page(2, 10)
            .and(vec![
                Filter::like("name", "hood"),
                Filter::like("sku", "hood"),
            ])
            .and(vec![Filter::eq("status", "1")])
            .sort("created_at", "DESC")
            .params();
        let get = |k: &str| p.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        assert_eq!(
            get("searchCriteria[filter_groups][0][filters][1][field]"),
            Some("sku")
        );
        assert_eq!(
            get("searchCriteria[filter_groups][0][filters][0][value]"),
            Some("%hood%")
        );
        assert_eq!(
            get("searchCriteria[filter_groups][1][filters][0][condition_type]"),
            Some("eq")
        );
        assert_eq!(
            get("searchCriteria[sortOrders][0][direction]"),
            Some("DESC")
        );
        assert_eq!(get("searchCriteria[currentPage]"), Some("2"));
    }
}
