use std::net::SocketAddr;

use pi_sandbox_master::proxy::{HeaderRuleSource, ProxyConfig, ProxyHandle, headers};
use pi_sandbox_master::types::HeaderOverrideRule;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::test]
async fn proxy_injects_headers_and_expires_token() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    let upstream_task = tokio::spawn(async move {
        let (mut socket, _) = upstream.accept().await.unwrap();
        let request = read_headers(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
        request
    });

    let proxy_addr = free_addr().await;
    let endpoint = format!("http://{}/*", upstream_addr);
    let global_rules = headers::resolve_rules(
        &[HeaderOverrideRule {
            endpoint: endpoint.clone(),
            override_header_key: Some("Authorization".into()),
            override_header_value: Some("Bearer global".into()),
            http_proxy: None,
            https_proxy: None,
        }],
        HeaderRuleSource::Global,
    )
    .unwrap();
    let proxy = ProxyHandle::start(ProxyConfig {
        listen_addr: proxy_addr,
        global_rules,
        mitm: None,
    })
    .await
    .unwrap();
    let registration = proxy
        .register_run(
            "run-1",
            "slot-1",
            &[
                HeaderOverrideRule {
                    endpoint: endpoint.clone(),
                    override_header_key: Some("authorization".into()),
                    override_header_value: Some("Bearer run".into()),
                    http_proxy: None,
                    https_proxy: None,
                },
                HeaderOverrideRule {
                    endpoint,
                    override_header_key: Some("X-Run-Header".into()),
                    override_header_value: Some("run-value".into()),
                    http_proxy: None,
                    https_proxy: None,
                },
            ],
        )
        .unwrap();
    let proxy_auth = proxy_auth_from_env(&registration.env());

    let response = send_proxy_request(proxy_addr, upstream_addr, &proxy_auth)
        .await
        .unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let upstream_request = upstream_task.await.unwrap();
    let upstream_request_lower = upstream_request.to_ascii_lowercase();
    assert!(upstream_request_lower.contains("authorization: bearer run\r\n"));
    assert!(upstream_request_lower.contains("x-run-header: run-value\r\n"));
    assert!(!upstream_request.contains("Proxy-Authorization"));

    drop(registration);
    let expired_response = send_proxy_request(proxy_addr, upstream_addr, &proxy_auth)
        .await
        .unwrap();
    assert!(expired_response.starts_with("HTTP/1.1 502 Bad Gateway"));
}

#[tokio::test]
async fn proxy_routes_matched_http_request_through_upstream_proxy() {
    let upstream_proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_proxy_addr = upstream_proxy.local_addr().unwrap();
    let upstream_proxy_task = tokio::spawn(async move {
        let (mut socket, _) = upstream_proxy.accept().await.unwrap();
        let request = read_headers(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nproxied")
            .await
            .unwrap();
        request
    });

    let proxy_addr = free_addr().await;
    let target_addr = free_addr().await;
    let endpoint = format!("http://{}/*", target_addr);
    let global_rules = headers::resolve_rules(
        &[HeaderOverrideRule {
            endpoint: endpoint.clone(),
            override_header_key: Some("X-Global".into()),
            override_header_value: Some("global".into()),
            http_proxy: Some(format!("http://{upstream_proxy_addr}")),
            https_proxy: None,
        }],
        HeaderRuleSource::Global,
    )
    .unwrap();
    let proxy = ProxyHandle::start(ProxyConfig {
        listen_addr: proxy_addr,
        global_rules,
        mitm: None,
    })
    .await
    .unwrap();
    let registration = proxy.register_run("run-1", "slot-1", &[]).unwrap();
    let proxy_auth = proxy_auth_from_env(&registration.env());

    let response = send_proxy_request(proxy_addr, target_addr, &proxy_auth)
        .await
        .unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let upstream_proxy_request = upstream_proxy_task.await.unwrap();
    let request_lower = upstream_proxy_request.to_ascii_lowercase();
    assert!(request_lower.starts_with(&format!("get http://{target_addr}/api/test http/1.1")));
    assert!(request_lower.contains("x-global: global\r\n"));
    assert!(!upstream_proxy_request.contains("Proxy-Authorization"));
}

async fn free_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap()
}

fn proxy_auth_from_env(env: &[(String, String)]) -> String {
    let proxy_url = env
        .iter()
        .find(|(key, _)| key == "HTTP_PROXY")
        .unwrap()
        .1
        .as_str();
    let token = proxy_url
        .strip_prefix("http://run:")
        .unwrap()
        .split('@')
        .next()
        .unwrap();
    format!(
        "Basic {}",
        base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("run:{token}")
        )
    )
}

async fn send_proxy_request(
    proxy_addr: SocketAddr,
    upstream_addr: SocketAddr,
    proxy_auth: &str,
) -> std::io::Result<String> {
    let mut socket = TcpStream::connect(proxy_addr).await?;
    let request = format!(
        "GET http://{upstream_addr}/api/test HTTP/1.1\r\nHost: {upstream_addr}\r\nProxy-Authorization: {proxy_auth}\r\nConnection: close\r\n\r\n"
    );
    socket.write_all(request.as_bytes()).await?;
    let mut response = String::new();
    socket.read_to_string(&mut response).await?;
    Ok(response)
}

async fn read_headers(socket: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 256];
    loop {
        let n = socket.read(&mut tmp).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(buf).unwrap()
}
