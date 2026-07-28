use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;

#[derive(Debug, Clone)]
pub enum EndpointMatcher {
    Glob(GlobMatcher),
    Regex(Regex),
}

impl EndpointMatcher {
    pub fn new(pattern: &str) -> Result<Self> {
        if pattern.len() > 2048 {
            bail!("endpoint pattern exceeds 2048 bytes");
        }
        if pattern.is_empty() {
            bail!("endpoint pattern must not be empty");
        }
        if let Some(regex) = pattern.strip_prefix("regex:") {
            if regex.is_empty() {
                bail!("regex endpoint pattern must not be empty");
            }
            return Ok(Self::Regex(
                Regex::new(regex).context("invalid regex endpoint pattern")?,
            ));
        }
        Ok(Self::Glob(
            GlobBuilder::new(pattern)
                .literal_separator(false)
                .backslash_escape(true)
                .build()
                .context("invalid glob endpoint pattern")?
                .compile_matcher(),
        ))
    }

    pub fn is_match(&self, normalized_url: &str) -> bool {
        match self {
            Self::Glob(glob) => glob.is_match(normalized_url),
            Self::Regex(regex) => regex.is_match(normalized_url),
        }
    }
}

pub fn normalize_uri(uri: &http::Uri) -> Result<String> {
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| anyhow::anyhow!("proxy request target must include scheme"))?
        .to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        bail!("unsupported proxy target scheme: {scheme}");
    }
    let authority = uri
        .authority()
        .ok_or_else(|| anyhow::anyhow!("proxy request target must include authority"))?;
    let host = authority.host().to_ascii_lowercase();
    if host.is_empty() {
        bail!("proxy request target host must not be empty");
    }
    let port = authority.port_u16();
    let include_port = !matches!(
        (scheme.as_str(), port),
        ("http", Some(80)) | ("https", Some(443)) | (_, None)
    );
    let path_and_query = uri.path_and_query().map(|v| v.as_str()).unwrap_or("/");
    let mut out = format!("{scheme}://{host}");
    if include_port {
        out.push(':');
        out.push_str(&port.expect("port checked").to_string());
    }
    out.push_str(path_and_query);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_normalized_url() {
        let matcher = EndpointMatcher::new("https://google.com/*").unwrap();
        assert!(matcher.is_match("https://google.com/search"));
        assert!(!matcher.is_match("https://evilgoogle.com/search"));
    }

    #[test]
    fn regex_matches_normalized_url() {
        let matcher = EndpointMatcher::new("regex:^https://google\\.com/api/.*$").unwrap();
        assert!(matcher.is_match("https://google.com/api/v1"));
        assert!(!matcher.is_match("https://google.com/other"));
    }

    #[test]
    fn normalize_omits_default_port() {
        let uri: http::Uri = "HTTP://Example.COM:80/a?b=c".parse().unwrap();
        assert_eq!(normalize_uri(&uri).unwrap(), "http://example.com/a?b=c");
    }

    #[test]
    fn rejects_empty_oversized_and_invalid_patterns() {
        assert!(EndpointMatcher::new("").is_err());
        assert!(EndpointMatcher::new("regex:").is_err());
        assert!(EndpointMatcher::new("regex:[").is_err());
        assert!(EndpointMatcher::new(&"x".repeat(2049)).is_err());
        assert!(EndpointMatcher::new(&"x".repeat(2048)).is_ok());
    }

    #[test]
    fn normalize_handles_root_non_default_ports_and_ipv6() {
        let cases = [
            ("https://Example.COM", "https://example.com/"),
            (
                "https://Example.COM:8443/Path?Q=Value",
                "https://example.com:8443/Path?Q=Value",
            ),
            ("https://Example.COM:443/a", "https://example.com/a"),
            ("http://[::1]:8080/a", "http://[::1]:8080/a"),
            ("https://[2001:DB8::1]:443/", "https://[2001:db8::1]/"),
        ];

        for (input, expected) in cases {
            let uri: http::Uri = input.parse().unwrap();
            assert_eq!(normalize_uri(&uri).unwrap(), expected);
        }
    }

    #[test]
    fn normalize_rejects_relative_and_unsupported_targets() {
        let relative: http::Uri = "/relative/path".parse().unwrap();
        assert!(normalize_uri(&relative).is_err());

        let unsupported: http::Uri = "ftp://example.com/file".parse().unwrap();
        assert!(normalize_uri(&unsupported).is_err());
    }
}
