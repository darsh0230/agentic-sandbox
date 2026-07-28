use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use http::{HeaderMap, HeaderName, HeaderValue};

use crate::types::HeaderOverrideRule;

use super::matcher::EndpointMatcher;
use super::{HeaderRuleSource, ResolvedHeaderOverrideRule};

pub const MAX_PER_RUN_RULES: usize = 128;
pub const MAX_GLOBAL_RULES: usize = 1024;

const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "proxy-connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

pub fn resolve_rules(
    rules: &[HeaderOverrideRule],
    source: HeaderRuleSource,
) -> Result<Vec<ResolvedHeaderOverrideRule>> {
    let max = match source {
        HeaderRuleSource::Global => MAX_GLOBAL_RULES,
        HeaderRuleSource::Run { .. } => MAX_PER_RUN_RULES,
    };
    if rules.len() > max {
        bail!("too many proxy header rules: max {max}");
    }
    rules
        .iter()
        .enumerate()
        .filter_map(|(index, rule)| {
            resolve_rule(rule, source.clone())
                .with_context(|| format!("rule {index}"))
                .transpose()
        })
        .collect()
}

pub fn apply_rules<'a, I>(headers: &mut HeaderMap, normalized_url: &str, rules: I) -> usize
where
    I: IntoIterator<Item = &'a ResolvedHeaderOverrideRule>,
{
    let mut matched = 0;
    for rule in rules {
        if rule.endpoint.is_match(normalized_url)
            && let (Some(header_key), Some(header_value)) = (&rule.header_key, &rule.header_value)
        {
            headers.insert(header_key.clone(), header_value.clone());
            matched += 1;
        }
    }
    matched
}

pub fn matching_upstream_proxy<'a, I>(
    normalized_url: &str,
    scheme: &str,
    rules: I,
) -> Option<String>
where
    I: IntoIterator<Item = &'a ResolvedHeaderOverrideRule>,
{
    rules
        .into_iter()
        .filter(|rule| rule.endpoint.is_match(normalized_url))
        .filter_map(|rule| rule.upstream_proxy_for_scheme(scheme))
        .last()
        .map(str::to_string)
}

pub fn remove_hop_by_hop(headers: &mut HeaderMap) {
    let mut connection_tokens = HashSet::new();
    for value in headers.get_all(http::header::CONNECTION) {
        if let Ok(value) = value.to_str() {
            for token in value.split(',') {
                connection_tokens.insert(token.trim().to_ascii_lowercase());
            }
        }
    }
    for name in HOP_BY_HOP_HEADERS {
        headers.remove(*name);
    }
    for token in connection_tokens {
        headers.remove(token.as_str());
    }
}

pub fn is_disallowed_override(name: &HeaderName) -> bool {
    name == http::header::HOST
        || HOP_BY_HOP_HEADERS
            .iter()
            .any(|header| name.as_str().eq_ignore_ascii_case(header))
}

fn resolve_rule(
    rule: &HeaderOverrideRule,
    source: HeaderRuleSource,
) -> Result<Option<ResolvedHeaderOverrideRule>> {
    validate_upstream_proxy(rule.http_proxy.as_deref()).context("invalid http_proxy")?;
    validate_upstream_proxy(rule.https_proxy.as_deref()).context("invalid https_proxy")?;
    let (header_key, header_value) = match (
        rule.override_header_key.as_deref(),
        rule.override_header_value.as_deref(),
    ) {
        (Some(key), Some(value)) => {
            if key.len() > 256 {
                bail!("header key exceeds 256 bytes");
            }
            if value.len() > 8192 {
                bail!("header value exceeds 8192 bytes");
            }
            if value.contains('\r') || value.contains('\n') {
                bail!("header value must not contain CR or LF");
            }
            let header_key: HeaderName = key.parse().context("invalid header name")?;
            if is_disallowed_override(&header_key) {
                bail!(
                    "cannot override hop-by-hop or Host header: {}",
                    header_key.as_str()
                );
            }
            let header_value: HeaderValue = value.parse().context("invalid header value")?;
            (Some(header_key), Some(header_value))
        }
        (None, None) => (None, None),
        _ => bail!("override_header_key and override_header_value must be provided together"),
    };
    if header_key.is_none() && rule.http_proxy.is_none() && rule.https_proxy.is_none() {
        return Ok(None);
    }
    Ok(Some(ResolvedHeaderOverrideRule {
        endpoint: EndpointMatcher::new(&rule.endpoint)?,
        header_key,
        header_value,
        http_proxy: rule.http_proxy.clone(),
        https_proxy: rule.https_proxy.clone(),
        source,
    }))
}

fn validate_upstream_proxy(proxy: Option<&str>) -> Result<()> {
    let Some(proxy) = proxy else {
        return Ok(());
    };
    if proxy.len() > 2048 {
        bail!("proxy URL exceeds 2048 bytes");
    }
    if proxy.contains('\r') || proxy.contains('\n') {
        bail!("proxy URL must not contain CR or LF");
    }
    let uri: http::Uri = proxy.parse().context("proxy URL is not a valid URI")?;
    match uri.scheme_str() {
        Some("http") | Some("https") => {}
        _ => bail!("proxy URL scheme must be http or https"),
    }
    if uri.authority().is_none() {
        bail!("proxy URL must include an authority");
    }
    Ok(())
}

impl ResolvedHeaderOverrideRule {
    pub fn upstream_proxy_for_scheme(&self, scheme: &str) -> Option<&str> {
        match scheme {
            "http" => self.http_proxy.as_deref(),
            "https" => self.https_proxy.as_deref(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(key: &str, value: &str) -> HeaderOverrideRule {
        HeaderOverrideRule {
            endpoint: "http://example.com/*".into(),
            override_header_key: Some(key.into()),
            override_header_value: Some(value.into()),
            http_proxy: None,
            https_proxy: None,
        }
    }

    fn proxy_rule(
        endpoint: &str,
        http_proxy: Option<&str>,
        https_proxy: Option<&str>,
    ) -> HeaderOverrideRule {
        HeaderOverrideRule {
            endpoint: endpoint.into(),
            override_header_key: None,
            override_header_value: None,
            http_proxy: http_proxy.map(str::to_string),
            https_proxy: https_proxy.map(str::to_string),
        }
    }

    #[test]
    fn rejects_hop_by_hop_and_host_overrides() {
        assert!(resolve_rules(&[rule("Connection", "x")], HeaderRuleSource::Global).is_err());
        assert!(resolve_rules(&[rule("Host", "x")], HeaderRuleSource::Global).is_err());
    }

    #[test]
    fn per_run_overrides_global_by_order() {
        let global = resolve_rules(&[rule("X-Test", "global")], HeaderRuleSource::Global).unwrap();
        let run = resolve_rules(
            &[rule("x-test", "run")],
            HeaderRuleSource::Run { run_id: "r".into() },
        )
        .unwrap();
        let mut headers = HeaderMap::new();
        apply_rules(&mut headers, "http://example.com/a", global.iter());
        apply_rules(&mut headers, "http://example.com/a", run.iter());
        assert_eq!(headers.get("x-test").unwrap(), "run");
    }

    #[test]
    fn rejects_crlf_values() {
        assert!(resolve_rules(&[rule("X-Test", "a\nb")], HeaderRuleSource::Global).is_err());
    }

    #[test]
    fn allows_proxy_only_rules() {
        let rules = resolve_rules(
            &[HeaderOverrideRule {
                endpoint: "http://example.com/*".into(),
                override_header_key: None,
                override_header_value: None,
                http_proxy: Some("http://proxy.example.com:80".into()),
                https_proxy: None,
            }],
            HeaderRuleSource::Global,
        )
        .unwrap();
        let mut headers = HeaderMap::new();
        assert_eq!(
            apply_rules(&mut headers, "http://example.com/a", rules.iter()),
            0
        );
        assert_eq!(
            matching_upstream_proxy("http://example.com/a", "http", rules.iter()).as_deref(),
            Some("http://proxy.example.com:80")
        );
    }

    #[test]
    fn rejects_incomplete_header_override_and_ignores_noop_rules() {
        assert!(
            resolve_rules(
                &[HeaderOverrideRule {
                    endpoint: "http://example.com/*".into(),
                    override_header_key: Some("X-Test".into()),
                    override_header_value: None,
                    http_proxy: None,
                    https_proxy: None,
                }],
                HeaderRuleSource::Global,
            )
            .is_err()
        );
        let rules = resolve_rules(
            &[HeaderOverrideRule {
                endpoint: "http://example.com/*".into(),
                override_header_key: None,
                override_header_value: None,
                http_proxy: None,
                https_proxy: None,
            }],
            HeaderRuleSource::Global,
        )
        .unwrap();
        assert!(rules.is_empty());
    }

    #[test]
    fn removes_standard_and_connection_nominated_hop_headers() {
        let mut headers = HeaderMap::new();
        headers.append(
            http::header::CONNECTION,
            "keep-alive, X-Remove".parse().unwrap(),
        );
        headers.append(http::header::CONNECTION, " x-another ".parse().unwrap());
        headers.insert("keep-alive", "timeout=5".parse().unwrap());
        headers.insert("proxy-authorization", "secret".parse().unwrap());
        headers.insert("x-remove", "one".parse().unwrap());
        headers.insert("x-another", "two".parse().unwrap());
        headers.insert("x-keep", "safe".parse().unwrap());

        remove_hop_by_hop(&mut headers);

        assert!(!headers.contains_key(http::header::CONNECTION));
        assert!(!headers.contains_key("keep-alive"));
        assert!(!headers.contains_key("proxy-authorization"));
        assert!(!headers.contains_key("x-remove"));
        assert!(!headers.contains_key("x-another"));
        assert_eq!(headers.get("x-keep").unwrap(), "safe");
    }

    #[test]
    fn rejects_all_disallowed_overrides_case_insensitively() {
        for name in HOP_BY_HOP_HEADERS {
            let mixed_case = name.to_ascii_uppercase();
            assert!(
                resolve_rules(&[rule(&mixed_case, "x")], HeaderRuleSource::Global).is_err(),
                "{mixed_case} should be rejected"
            );
        }
        assert!(resolve_rules(&[rule("hOsT", "x")], HeaderRuleSource::Global).is_err());
    }

    #[test]
    fn enforces_global_and_per_run_rule_limits() {
        let noop = HeaderOverrideRule {
            endpoint: "http://example.com/*".into(),
            override_header_key: None,
            override_header_value: None,
            http_proxy: None,
            https_proxy: None,
        };

        assert!(
            resolve_rules(
                &vec![noop.clone(); MAX_GLOBAL_RULES],
                HeaderRuleSource::Global
            )
            .is_ok()
        );
        assert!(
            resolve_rules(
                &vec![noop.clone(); MAX_GLOBAL_RULES + 1],
                HeaderRuleSource::Global
            )
            .is_err()
        );
        assert!(
            resolve_rules(
                &vec![noop.clone(); MAX_PER_RUN_RULES],
                HeaderRuleSource::Run { run_id: "r".into() }
            )
            .is_ok()
        );
        assert!(
            resolve_rules(
                &vec![noop; MAX_PER_RUN_RULES + 1],
                HeaderRuleSource::Run { run_id: "r".into() }
            )
            .is_err()
        );
    }

    #[test]
    fn validates_upstream_proxy_urls() {
        for proxy in [
            "",
            "proxy.example.com:8080",
            "socks5://proxy.example.com:1080",
            "http:/missing-authority",
            "http://proxy.example.com/\nX-Injected: yes",
        ] {
            assert!(
                resolve_rules(
                    &[proxy_rule("http://example.com/*", Some(proxy), None)],
                    HeaderRuleSource::Global,
                )
                .is_err(),
                "{proxy:?} should be rejected"
            );
        }

        assert!(
            resolve_rules(
                &[proxy_rule(
                    "https://example.com/*",
                    None,
                    Some("https://proxy.example.com:8443"),
                )],
                HeaderRuleSource::Global,
            )
            .is_ok()
        );
    }

    #[test]
    fn last_matching_proxy_for_the_requested_scheme_wins() {
        let rules = resolve_rules(
            &[
                proxy_rule(
                    "https://example.com/*",
                    Some("http://first.example:80"),
                    Some("http://first.example:80"),
                ),
                proxy_rule(
                    "https://example.com/api/*",
                    None,
                    Some("http://second.example:80"),
                ),
                proxy_rule(
                    "https://other.example/*",
                    None,
                    Some("http://other.example:80"),
                ),
            ],
            HeaderRuleSource::Global,
        )
        .unwrap();

        assert_eq!(
            matching_upstream_proxy("https://example.com/api/items", "https", &rules).as_deref(),
            Some("http://second.example:80")
        );
        assert_eq!(
            matching_upstream_proxy("https://example.com/api/items", "http", &rules).as_deref(),
            Some("http://first.example:80")
        );
        assert_eq!(
            matching_upstream_proxy("https://example.com/miss", "https", &rules).as_deref(),
            Some("http://first.example:80")
        );
        assert_eq!(
            matching_upstream_proxy("https://unmatched.example/", "https", &rules),
            None
        );
    }
}
