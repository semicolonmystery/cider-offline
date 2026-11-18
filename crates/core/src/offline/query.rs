//! Decoded Apple Music API query parameters (`include`, `fields[...]`,
//! `limit`/`offset`, `ids[...]` …) as Cider sends them.

use hudsucker::hyper::Uri;

/// Default page size of a top-level listing when `limit` is absent.
pub(crate) const DEFAULT_LIMIT: usize = 25;
/// Largest page size Apple accepts for library listings.
pub(crate) const MAX_LIMIT: usize = 100;

/// All query pairs of one request, percent-decoded, in order.
#[derive(Debug, Clone, Default)]
pub(crate) struct Query {
    pairs: Vec<(String, String)>,
}

impl Query {
    /// Parses the query of `uri` (form decoding, `+` is a space).
    pub(crate) fn parse(uri: &Uri) -> Self {
        let Some(query) = uri.query() else {
            return Self::default();
        };
        let Ok(url) = reqwest::Url::parse(&format!("http://q/?{query}")) else {
            return Self::default();
        };
        Self {
            pairs: url
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
        }
    }

    /// First value of `name`.
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Every comma-separated, non-empty value of every `name` parameter.
    pub(crate) fn list(&self, name: &str) -> Vec<&str> {
        self.pairs
            .iter()
            .filter(|(k, _)| k == name)
            .flat_map(|(_, v)| v.split(','))
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .collect()
    }

    /// A non-negative integer parameter.
    pub(crate) fn number(&self, name: &str) -> Option<usize> {
        self.get(name).and_then(|v| v.trim().parse().ok())
    }

    /// The `l` (locale) parameter.
    pub(crate) fn locale(&self) -> Option<&str> {
        self.get("l").filter(|l| !l.is_empty())
    }

    /// Relationships to include for resources of type `ty`
    /// (`include` plus `include[ty]`).
    pub(crate) fn includes(&self, ty: &str) -> Vec<&str> {
        let mut out = self.list("include");
        out.extend(self.list(&format!("include[{ty}]")));
        out
    }

    /// Whether `relationship` is requested for `ty` through `include`,
    /// `include[ty]` or `relate` / `relate[ty]`.
    pub(crate) fn wants(&self, ty: &str, relationship: &str) -> bool {
        self.includes(ty).contains(&relationship)
            || self.list("relate").contains(&relationship)
            || self.list(&format!("relate[{ty}]")).contains(&relationship)
    }

    /// Attribute whitelist for `ty`: `fields[ty]`, else plain `fields`
    /// (Apple applies a plain list to every resource of the response,
    /// included ones too).
    pub(crate) fn fields(&self, ty: &str) -> Option<Vec<&str>> {
        let typed = format!("fields[{ty}]");
        if self.pairs.iter().any(|(k, _)| *k == typed) {
            return Some(self.list(&typed));
        }
        self.pairs
            .iter()
            .any(|(k, _)| k == "fields")
            .then(|| self.list("fields"))
    }

    /// Extended attributes requested for `ty` (`extend` plus `extend[ty]`).
    pub(crate) fn extends(&self, ty: &str) -> Vec<&str> {
        let mut out = self.list("extend");
        out.extend(self.list(&format!("extend[{ty}]")));
        out
    }

    /// `(offset, limit)` of a top-level listing, `limit` clamped to `1..=max`.
    pub(crate) fn page(&self, default_limit: usize, max: usize) -> (usize, usize) {
        let limit = self
            .number("limit")
            .unwrap_or(default_limit)
            .clamp(1, max.max(1));
        (self.number("offset").unwrap_or(0), limit)
    }

    /// `ids[type]=a,b` parameters of a multi-type lookup, in request order.
    pub(crate) fn typed_ids(&self) -> Vec<(String, Vec<String>)> {
        let mut out: Vec<(String, Vec<String>)> = Vec::new();
        for (key, _) in &self.pairs {
            let Some(ty) = key
                .strip_prefix("ids[")
                .and_then(|rest| rest.strip_suffix(']'))
            else {
                continue;
            };
            if out.iter().any(|(t, _)| t == ty) {
                continue;
            }
            let ids = self.list(key).into_iter().map(str::to_owned).collect();
            out.push((ty.to_owned(), ids));
        }
        out
    }

    /// Whether the normalized `format[resources]=map` form was requested.
    pub(crate) fn wants_resource_map(&self) -> bool {
        self.get("format[resources]") == Some("map")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(s: &str) -> Query {
        Query::parse(&format!("https://h/p?{s}").parse().unwrap())
    }

    #[test]
    fn list_should_merge_repeated_and_comma_values() {
        assert_eq!(
            query("include=a,b&include=c").list("include"),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn typed_fields_should_win_over_plain_fields() {
        assert_eq!(
            query("fields=name&fields%5Bsongs%5D=url").fields("songs"),
            Some(vec!["url"])
        );
    }

    #[test]
    fn plain_fields_should_apply_to_any_type() {
        assert_eq!(query("fields=name").fields("albums"), Some(vec!["name"]));
    }

    #[test]
    fn page_should_clamp_limit() {
        assert_eq!(query("offset=100&limit=500").page(25, 100), (100, 100));
    }

    #[test]
    fn typed_ids_should_parse_bracket_keys() {
        assert_eq!(
            query("ids%5Bsongs%5D=1,2&ids%5Balbums%5D=3").typed_ids(),
            vec![
                ("songs".to_owned(), vec!["1".to_owned(), "2".to_owned()]),
                ("albums".to_owned(), vec!["3".to_owned()])
            ]
        );
    }

    #[test]
    fn relate_should_count_as_wanted() {
        assert!(query("relate=library").wants("songs", "library"));
    }
}
