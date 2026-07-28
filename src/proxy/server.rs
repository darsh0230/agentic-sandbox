use std::convert::Infallible;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use http::{HeaderMap, Method, Request, Response, StatusCode, Uri, header};
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::upgrade::Upgraded;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::sync::Arc;
use tokio::io::copy_bidirectional;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, warn};

use super::headers;
use super::matcher::normalize_uri;
use super::{ProxyHandle, ProxyRunContext, ResolvedHeaderOverrideRule};

type ProxyBody = BoxBody<Bytes, hyper::Error>;
type ProxyClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Incoming>;

pub async fn spawn(handle: ProxyHandle, listener: TcpListener) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let local_addr = listener.local_addr()?;
    if local_addr != handle.listen_addr() {
        bail!("proxy listener bound unexpected address {local_addr}");
    }
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()?
        .https_or_http()
        .enable_http1()
        .build();
    let client: ProxyClient = Client::builder(TokioExecutor::new()).build(https);
    tokio::spawn(async move {
        loop {
            let Ok((stream, peer_addr)) = listener.accept().await else {
                continue;
            };
            let handle = handle.clone();
            let client = client.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service =
                    service_fn(move |req| proxy_request(req, handle.clone(), client.clone()));
                if let Err(err) = hyper::server::conn::http1::Builder::new()
                    .preserve_header_case(true)
                    .title_case_headers(true)
                    .serve_connection(io, service)
                    .with_upgrades()
                    .await
                {
                    debug!(%peer_addr, error = %err, "proxy connection failed");
                }
            });
        }
    });
    Ok(())
}

async fn proxy_request(
    req: Request<Incoming>,
    handle: ProxyHandle,
    client: ProxyClient,
) -> Result<Response<ProxyBody>, Infallible> {
    let started = Instant::now();
    let result = if req.method() == Method::CONNECT {
        handle_connect(req, handle, client).await
    } else {
        handle_http(req, handle, client, started).await
    };
    Ok(match result {
        Ok(response) => response,
        Err(err) => {
            warn!(error = %err, "proxy request rejected");
            response(StatusCode::BAD_GATEWAY, "proxy request failed")
        }
    })
}

async fn handle_http(
    req: Request<Incoming>,
    handle: ProxyHandle,
    client: ProxyClient,
    started: Instant,
) -> Result<Response<ProxyBody>> {
    let context = authenticate(req.headers(), &handle)?;
    let normalized_url = normalize_uri(req.uri())?;
    let target_uri = req.uri().clone();
    if target_uri.scheme_str() != Some("http") {
        bail!("only plaintext http requests can be forwarded without CONNECT");
    }

    let (mut parts, body) = req.into_parts();
    headers::remove_hop_by_hop(&mut parts.headers);
    let matched_global = headers::apply_rules(
        &mut parts.headers,
        &normalized_url,
        handle.global_rules().iter(),
    );
    let matched_run =
        headers::apply_rules(&mut parts.headers, &normalized_url, context.rules.iter());
    let upstream_proxy = selected_upstream_proxy(
        &normalized_url,
        "http",
        handle.global_rules(),
        &context.rules,
    );
    parts.headers.insert(
        header::HOST,
        header_value_for_host(&target_uri).context("build Host header")?,
    );
    parts.uri = target_uri;

    let method = parts.method.clone();
    let mut upstream = if let Some(upstream_proxy) = upstream_proxy.as_deref() {
        forward_with_upstream_proxy(Request::from_parts(parts, body), upstream_proxy).await?
    } else {
        client
            .request(Request::from_parts(parts, body))
            .await?
            .map(|body| body.boxed())
    };
    headers::remove_hop_by_hop(upstream.headers_mut());
    let status = upstream.status();
    info!(
        run_id = %context.run_id,
        slot_id = %context.slot_id,
        %method,
        target = %redact_query(&normalized_url),
        matched_global,
        matched_run,
        upstream_proxy = upstream_proxy.is_some(),
        status = status.as_u16(),
        duration_ms = started.elapsed().as_millis(),
        "proxy request forwarded"
    );
    Ok(upstream)
}

async fn handle_connect(
    req: Request<Incoming>,
    handle: ProxyHandle,
    client: ProxyClient,
) -> Result<Response<ProxyBody>> {
    let context = authenticate(req.headers(), &handle)?;
    let target_authority = req
        .uri()
        .authority()
        .ok_or_else(|| anyhow::anyhow!("CONNECT target must be host:port"))?
        .clone();
    let target = target_authority.as_str().to_string();
    let connect_url = normalized_connect_url(&target_authority);
    let matched_global = count_matches(handle.global_rules(), &connect_url);
    let matched_run = count_matches(&context.rules, &connect_url);
    let upstream_proxy =
        selected_upstream_proxy(&connect_url, "https", handle.global_rules(), &context.rules);
    if let Some(authority) = handle.mitm() {
        info!(
            run_id = %context.run_id,
            slot_id = %context.slot_id,
            target = %target,
            match_probe_url = %connect_url,
            matched_global,
            matched_run,
            upstream_proxy = upstream_proxy.is_some(),
            "HTTPS CONNECT intercepted by proxy MITM"
        );
        let global_rules = handle.global_rules().to_vec();
        tokio::spawn(async move {
            if let Err(err) = mitm_connect(
                req,
                authority,
                context,
                global_rules,
                target_authority,
                client,
            )
            .await
            {
                debug!(error = %err, "CONNECT MITM failed");
            }
        });
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .body(empty_body())?);
    }
    info!(
        run_id = %context.run_id,
        slot_id = %context.slot_id,
        target = %target,
        match_probe_url = %connect_url,
        matched_global,
        matched_run,
        upstream_proxy = upstream_proxy.is_some(),
        "HTTPS CONNECT tunneled without header injection; encrypted request path and headers are not visible"
    );

    tokio::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let result = if let Some(upstream_proxy) = upstream_proxy {
                    tunnel_via_upstream_proxy(upgraded, target, upstream_proxy).await
                } else {
                    tunnel(upgraded, target).await
                };
                if let Err(err) = result {
                    debug!(error = %err, "CONNECT tunnel failed");
                }
            }
            Err(err) => debug!(error = %err, "CONNECT upgrade failed"),
        }
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .body(empty_body())?)
}

async fn mitm_connect(
    req: Request<Incoming>,
    authority: Arc<super::mitm::MitmAuthority>,
    context: ProxyRunContext,
    global_rules: Vec<ResolvedHeaderOverrideRule>,
    target: http::uri::Authority,
    client: ProxyClient,
) -> Result<()> {
    let host = target.host().to_ascii_lowercase();
    let leaf = authority.certificate_for_host(&host)?;
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(leaf.cert_der)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf.key_der)),
        )?;
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let upgraded = hyper::upgrade::on(req).await?;
    let tls = acceptor.accept(TokioIo::new(upgraded)).await?;
    let io = TokioIo::new(tls);
    let service = service_fn(move |req| {
        mitm_https_request(
            req,
            client.clone(),
            context.clone(),
            global_rules.clone(),
            target.clone(),
        )
    });
    hyper::server::conn::http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .serve_connection(io, service)
        .await?;
    Ok(())
}

async fn mitm_https_request(
    req: Request<Incoming>,
    client: ProxyClient,
    context: ProxyRunContext,
    global_rules: Vec<ResolvedHeaderOverrideRule>,
    target: http::uri::Authority,
) -> Result<Response<ProxyBody>, Infallible> {
    let started = Instant::now();
    let result = forward_mitm_https(req, client, context, global_rules, target, started).await;
    Ok(match result {
        Ok(response) => response,
        Err(err) => {
            warn!(error = %err, "proxy MITM request rejected");
            response(StatusCode::BAD_GATEWAY, "proxy MITM request failed")
        }
    })
}

async fn forward_mitm_https(
    req: Request<Incoming>,
    client: ProxyClient,
    context: ProxyRunContext,
    global_rules: Vec<ResolvedHeaderOverrideRule>,
    target: http::uri::Authority,
    started: Instant,
) -> Result<Response<ProxyBody>> {
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    let upstream_uri: Uri = format!("https://{target}{path_and_query}").parse()?;
    let normalized_url = normalize_uri(&upstream_uri)?;
    let (mut parts, body) = req.into_parts();
    headers::remove_hop_by_hop(&mut parts.headers);
    let matched_global =
        headers::apply_rules(&mut parts.headers, &normalized_url, global_rules.iter());
    let matched_run =
        headers::apply_rules(&mut parts.headers, &normalized_url, context.rules.iter());
    let upstream_proxy =
        selected_upstream_proxy(&normalized_url, "https", &global_rules, &context.rules);
    parts.headers.insert(
        header::HOST,
        header_value_for_host(&upstream_uri).context("build Host header")?,
    );
    parts.uri = upstream_uri;
    let method = parts.method.clone();
    let mut upstream = if let Some(upstream_proxy) = upstream_proxy.as_deref() {
        forward_with_upstream_proxy(Request::from_parts(parts, body), upstream_proxy).await?
    } else {
        client
            .request(Request::from_parts(parts, body))
            .await?
            .map(|body| body.boxed())
    };
    headers::remove_hop_by_hop(upstream.headers_mut());
    let status = upstream.status();
    info!(
        run_id = %context.run_id,
        slot_id = %context.slot_id,
        %method,
        target = %redact_query(&normalized_url),
        matched_global,
        matched_run,
        upstream_proxy = upstream_proxy.is_some(),
        status = status.as_u16(),
        duration_ms = started.elapsed().as_millis(),
        "proxy MITM HTTPS request forwarded"
    );
    Ok(upstream)
}

fn normalized_connect_url(authority: &http::uri::Authority) -> String {
    let host = authority.host().to_ascii_lowercase();
    match authority.port_u16() {
        Some(443) | None => format!("https://{host}/"),
        Some(port) => format!("https://{host}:{port}/"),
    }
}

fn count_matches(rules: &[ResolvedHeaderOverrideRule], normalized_url: &str) -> usize {
    rules
        .iter()
        .filter(|rule| rule.endpoint.is_match(normalized_url))
        .count()
}

fn selected_upstream_proxy(
    normalized_url: &str,
    scheme: &str,
    global_rules: &[ResolvedHeaderOverrideRule],
    run_rules: &[ResolvedHeaderOverrideRule],
) -> Option<String> {
    headers::matching_upstream_proxy(normalized_url, scheme, run_rules.iter())
        .or_else(|| headers::matching_upstream_proxy(normalized_url, scheme, global_rules.iter()))
}

async fn forward_with_upstream_proxy(
    req: Request<Incoming>,
    upstream_proxy: &str,
) -> Result<Response<ProxyBody>> {
    let (parts, body) = req.into_parts();
    let body = body.collect().await?.to_bytes();
    let proxy = match parts.uri.scheme_str() {
        Some("http") => reqwest::Proxy::http(upstream_proxy)?,
        Some("https") => reqwest::Proxy::https(upstream_proxy)?,
        Some(scheme) => bail!("unsupported proxied request scheme: {scheme}"),
        None => bail!("proxied request URI missing scheme"),
    };
    let client = reqwest::Client::builder().no_proxy().proxy(proxy).build()?;
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())?;
    let mut builder = client.request(method, parts.uri.to_string());
    for (name, value) in parts.headers.iter() {
        if name == header::HOST || name == header::CONTENT_LENGTH {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_bytes());
    }
    let response = builder.body(body.to_vec()).send().await?;
    let status = response.status().as_u16();
    let mut builder = Response::builder().status(status);
    for (name, value) in response.headers() {
        builder = builder.header(name.as_str(), value.as_bytes());
    }
    let body = response.bytes().await?;
    Ok(builder.body(full_bytes_body(body))?)
}

async fn tunnel(upgraded: Upgraded, target: String) -> Result<()> {
    let mut upgraded = TokioIo::new(upgraded);
    let mut server = TcpStream::connect(&target)
        .await
        .with_context(|| format!("connect CONNECT target {target}"))?;
    copy_bidirectional(&mut upgraded, &mut server).await?;
    Ok(())
}

async fn tunnel_via_upstream_proxy(
    upgraded: Upgraded,
    target: String,
    upstream_proxy: String,
) -> Result<()> {
    let proxy_uri: Uri = upstream_proxy.parse().context("parse upstream proxy URL")?;
    if proxy_uri.scheme_str() != Some("http") {
        bail!("CONNECT tunneling supports http upstream proxy URLs only");
    }
    let proxy_authority = proxy_uri
        .authority()
        .ok_or_else(|| anyhow::anyhow!("upstream proxy URL missing authority"))?;
    let proxy_target = proxy_authority.as_str();
    let mut upgraded = TokioIo::new(upgraded);
    let mut server = TcpStream::connect(proxy_target)
        .await
        .with_context(|| format!("connect upstream proxy {proxy_target}"))?;
    let request = format!(
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Connection: Keep-Alive\r\n\r\n"
    );
    server.write_all(request.as_bytes()).await?;

    let mut response = Vec::new();
    let mut buf = [0_u8; 1024];
    loop {
        let n = server.read(&mut buf).await?;
        if n == 0 {
            bail!("upstream proxy closed before CONNECT response");
        }
        response.extend_from_slice(&buf[..n]);
        if response.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if response.len() > 16 * 1024 {
            bail!("upstream proxy CONNECT response too large");
        }
    }
    let response = std::str::from_utf8(&response).context("upstream proxy CONNECT response")?;
    if !response.starts_with("HTTP/1.1 2") && !response.starts_with("HTTP/1.0 2") {
        bail!("upstream proxy CONNECT failed");
    }
    copy_bidirectional(&mut upgraded, &mut server).await?;
    Ok(())
}

fn authenticate(headers: &HeaderMap, handle: &ProxyHandle) -> Result<ProxyRunContext> {
    let header = headers
        .get(header::PROXY_AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow::anyhow!("missing Proxy-Authorization"))?;
    let encoded = header
        .strip_prefix("Basic ")
        .ok_or_else(|| anyhow::anyhow!("unsupported proxy authorization scheme"))?;
    let decoded = BASE64
        .decode(encoded)
        .context("invalid proxy authorization")?;
    let decoded = std::str::from_utf8(&decoded).context("proxy authorization is not UTF-8")?;
    let token = decoded
        .strip_prefix("run:")
        .ok_or_else(|| anyhow::anyhow!("invalid proxy authorization username"))?;
    handle
        .run_context(token)
        .ok_or_else(|| anyhow::anyhow!("unknown or expired proxy token"))
}

fn header_value_for_host(uri: &Uri) -> Result<http::HeaderValue> {
    let authority = uri
        .authority()
        .ok_or_else(|| anyhow::anyhow!("target URI missing authority"))?;
    let host = authority.host().to_ascii_lowercase();
    let port = authority.port_u16();
    let value = match (uri.scheme_str(), port) {
        (Some("http"), Some(80)) | (Some("https"), Some(443)) | (_, None) => host,
        (_, Some(port)) => format!("{host}:{port}"),
    };
    Ok(value.parse()?)
}

fn response(status: StatusCode, message: &'static str) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .body(full_body(message))
        .unwrap_or_else(|err| {
            error!(error = %err, "failed to build proxy error response");
            Response::new(empty_body())
        })
}

fn empty_body() -> ProxyBody {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

fn full_body(message: &'static str) -> ProxyBody {
    Full::new(Bytes::from_static(message.as_bytes()))
        .map_err(|never| match never {})
        .boxed()
}

fn full_bytes_body(bytes: Bytes) -> ProxyBody {
    Full::new(bytes).map_err(|never| match never {}).boxed()
}

fn redact_query(url: &str) -> String {
    match url.split_once('?') {
        Some((prefix, _)) => format!("{prefix}?<redacted>"),
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::RwLock;

    use crate::types::HeaderOverrideRule;

    use super::*;

    #[test]
    fn redacts_query_strings() {
        assert_eq!(redact_query("http://a/b?secret=x"), "http://a/b?<redacted>");
        assert_eq!(redact_query("http://a/b"), "http://a/b");
    }

    #[test]
    fn connect_probe_url_omits_default_https_port() {
        let authority: http::uri::Authority = "Example.COM:443".parse().unwrap();
        assert_eq!(normalized_connect_url(&authority), "https://example.com/");
    }

    #[test]
    fn connect_probe_url_preserves_non_default_ports_and_ipv6() {
        let non_default: http::uri::Authority = "Example.COM:8443".parse().unwrap();
        assert_eq!(
            normalized_connect_url(&non_default),
            "https://example.com:8443/"
        );

        let ipv6: http::uri::Authority = "[2001:DB8::1]:8443".parse().unwrap();
        assert_eq!(normalized_connect_url(&ipv6), "https://[2001:db8::1]:8443/");
    }

    #[test]
    fn host_header_omits_only_default_ports_and_preserves_ipv6_brackets() {
        let cases = [
            ("http://Example.COM:80/a", "example.com"),
            ("https://Example.COM:443/a", "example.com"),
            ("http://Example.COM:8080/a", "example.com:8080"),
            ("https://[::1]:8443/a", "[::1]:8443"),
            ("https://[::1]/a", "[::1]"),
        ];

        for (uri, expected) in cases {
            let uri: Uri = uri.parse().unwrap();
            assert_eq!(header_value_for_host(&uri).unwrap(), expected);
        }
    }

    #[test]
    fn run_upstream_proxy_takes_precedence_over_global_proxy() {
        fn proxy_rule(proxy: &str) -> HeaderOverrideRule {
            HeaderOverrideRule {
                endpoint: "https://example.com/*".into(),
                override_header_key: None,
                override_header_value: None,
                http_proxy: None,
                https_proxy: Some(proxy.into()),
            }
        }

        let global = headers::resolve_rules(
            &[proxy_rule("http://global.example:8080")],
            super::super::HeaderRuleSource::Global,
        )
        .unwrap();
        let run = headers::resolve_rules(
            &[
                proxy_rule("http://run-first.example:8080"),
                proxy_rule("http://run-last.example:8080"),
            ],
            super::super::HeaderRuleSource::Run { run_id: "r".into() },
        )
        .unwrap();

        assert_eq!(
            selected_upstream_proxy("https://example.com/a", "https", &global, &run).as_deref(),
            Some("http://run-last.example:8080")
        );
        assert_eq!(
            selected_upstream_proxy("https://example.com/a", "https", &global, &[]).as_deref(),
            Some("http://global.example:8080")
        );
    }

    #[test]
    fn proxy_authentication_accepts_only_live_run_credentials() {
        let handle = ProxyHandle {
            listen_addr: "127.0.0.1:18080".parse().unwrap(),
            global_rules: Arc::new(Vec::new()),
            contexts: Arc::new(RwLock::new(HashMap::new())),
            mitm: None,
        };
        let registration = handle.register_run("run-1", "slot-1", &[]).unwrap();

        let mut headers = HeaderMap::new();
        let encoded = BASE64.encode(format!("run:{}", registration.token));
        headers.insert(
            header::PROXY_AUTHORIZATION,
            format!("Basic {encoded}").parse().unwrap(),
        );
        let context = authenticate(&headers, &handle).unwrap();
        assert_eq!(context.run_id, "run-1");

        let mut invalid = HeaderMap::new();
        for value in [
            "Bearer token".to_string(),
            "Basic !!!".to_string(),
            format!("Basic {}", BASE64.encode("other:user")),
            format!("Basic {}", BASE64.encode("run:unknown")),
        ] {
            invalid.insert(header::PROXY_AUTHORIZATION, value.parse().unwrap());
            assert!(authenticate(&invalid, &handle).is_err(), "{value}");
        }
        assert!(authenticate(&HeaderMap::new(), &handle).is_err());

        drop(registration);
        assert!(authenticate(&headers, &handle).is_err());
    }
}
