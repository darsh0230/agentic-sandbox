# Master-Controlled Proxy Header Injection Implementation Plan

## Goal

Add a master-controlled outbound proxy that agents use through proxy environment variables. The proxy will inspect HTTP API requests made by an agent, match the target endpoint against per-run and global header override rules, and inject or override configured request headers before forwarding the request.

The feature must work in both sandbox isolation modes:

- `privileged`: agent runs inside the synthetic rootfs, but shares the container network namespace.
- `user`: agent runs as the per-slot UID/GID in the container filesystem, and also shares the container network namespace.

This plan intentionally implements Approach 1 from the feasibility review: a forward proxy configured through `HTTP_PROXY` and `HTTPS_PROXY`. It is production-ready for cooperative agent clients, but it is not a hard network enforcement boundary because agents can technically bypass environment proxy settings with direct sockets.

## Non-Goals

- Do not implement iptables, nftables, TPROXY, eBPF, cgroups, or per-agent network namespaces in this phase.
- Do not guarantee interception of traffic from clients that ignore `HTTP_PROXY` or `HTTPS_PROXY`.
- Do not implement full HTTPS MITM in the first implementation unless explicit HTTPS payload/header injection is required. Without MITM, HTTPS `CONNECT` requests can be tunneled but their encrypted HTTP headers cannot be modified.
- Do not expose master credentials to agents through environment variables, files, command arguments, stdout, stderr, or result files.

## Current Codebase Fit

- `src/types.rs` owns the `/api/run` request schema via `AgentRunRequest`.
- `src/api/http.rs` accepts `POST /api/run` and deserializes JSON directly into `AgentRunRequest`.
- `src/master/mod.rs` prepares workspace, injects files, builds the clean environment, and spawns the child via `SandboxLauncher`.
- `src/policy/defaults.rs` builds agent env with `env_clear()` upstream in `src/sandbox/launcher.rs`, which is the correct insertion point for `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and proxy-related TLS env vars.
- Agents in both isolation modes share the container network namespace, so a master proxy listening on `127.0.0.1:<port>` is reachable from agents in both modes.
- Agents are denied `bind`, `listen`, `accept`, and `accept4` by seccomp, but can make outbound `connect` calls, so forward proxy usage works.

## API Changes

Extend `AgentRunRequest` in `src/types.rs` with a new optional field:

```rust
#[serde(default)]
pub inject_headers: Vec<HeaderOverrideRule>,
```

Add the rule type:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeaderOverrideRule {
    pub endpoint: String,
    pub override_header_key: String,
    pub override_header_value: String,
}
```

Example `/api/run` payload:

```json
{
  "run_id": "example-run",
  "command": ["pi", "--workspace", "/home/agent"],
  "inject_headers": [
    {
      "endpoint": "https://api.example.com/*",
      "override_header_key": "Authorization",
      "override_header_value": "Bearer per-run-token"
    },
    {
      "endpoint": "regex:^https://api\\.example\\.com/v[0-9]+/private/.*$",
      "override_header_key": "X-Run-Auth",
      "override_header_value": "run-scoped-value"
    }
  ]
}
```

### Endpoint Pattern Semantics

Support two explicit matching forms:

- Glob form: default. Example: `https://google.com/*`, `https://google.com/api/*`.
- Regex form: prefix with `regex:`. Example: `regex:^https://google\\.com/api/.*$`.

Matching should be against the normalized absolute target URL string:

```text
<scheme>://<host>[:port]<path>[?query]
```

Recommended normalization rules:

- Lowercase scheme and host.
- Preserve path and query exactly, except reject invalid URI forms.
- Default ports may be omitted from the normalized string (`:80` for HTTP, `:443` for HTTPS).
- Do not include fragments because HTTP requests do not send fragments.

### Header Override Precedence

Rules should be evaluated in this order:

1. Global rules from master-only config.
2. Per-run rules from `AgentRunRequest.inject_headers`.

When multiple matching rules set the same header key, later rules override earlier rules. Therefore per-run rules can override global rules unless a global rule is marked immutable in a later extension.

Header names should be compared case-insensitively and written using the configured key casing.

## Global Header Override Config

Add a master-only config file for global rules. The existing `--config` argument is currently only logged and not loaded (`src/main.rs:65-67`), so introduce a dedicated proxy config path rather than overloading the placeholder config in the first implementation.

Recommended CLI and env:

```rust
#[arg(long, env = "PI_SANDBOX_PROXY_CONFIG")]
proxy_config: Option<PathBuf>,
```

Recommended default path when not explicitly supplied:

```text
/var/lib/pi-sandbox/state/proxy-headers.json
```

If the default file does not exist, start with no global overrides. If an explicit path is supplied and cannot be read or parsed, fail startup.

### Global Config Format

Use JSON to match the existing request payload format and avoid adding TOML dependencies.

```json
{
  "global_inject_headers": [
    {
      "endpoint": "https://api.example.com/*",
      "override_header_key": "Authorization",
      "override_header_value": "env:EXAMPLE_API_TOKEN"
    },
    {
      "endpoint": "regex:^https://internal\\.example\\.com/api/.*$",
      "override_header_key": "X-Service-Key",
      "override_header_value": "literal-service-key"
    }
  ]
}
```

### Env Var Compatible Global Values

Only global override values support environment resolution. Per-run override values remain literal to avoid ambiguous request behavior and to keep master-side secret lookup controlled by configuration only.

Supported forms:

- `env:NAME`: resolve from the master process environment at config load time.
- `${NAME}`: resolve from the master process environment at config load time.
- Any other value: treat as a literal string.

Resolution rules:

- Resolve environment variables once at startup, not per request, to keep behavior deterministic.
- If a referenced env var is missing or empty, fail startup for explicit config paths and fail closed for default config when the file exists.
- Never log resolved secret values.
- In logs and validation errors, refer to the env var name only.

### Master-Only File Permissions

When loading the global config, enforce master-only readability on Linux/Unix:

- Owner UID must be `0` or the current master UID.
- File mode must not be group- or world-readable/writable. Recommended mode: `0600`.
- Parent directory should already be under `/var/lib/pi-sandbox/state`, which is created as master-owned `0700` by the Dockerfile.

If permissions are too broad, fail startup with a clear error. This is important because global config can reference secret env vars and may also contain literal secrets.

## Proxy Architecture

Add a new module tree:

```text
src/proxy/
  mod.rs
  config.rs
  matcher.rs
  server.rs
  headers.rs
```

### Core Types

```rust
pub struct ProxyConfig {
    pub listen_addr: SocketAddr,
    pub global_rules: Vec<ResolvedHeaderOverrideRule>,
}

pub struct ResolvedHeaderOverrideRule {
    pub endpoint: EndpointMatcher,
    pub header_key: HeaderName,
    pub header_value: HeaderValue,
    pub source: HeaderRuleSource,
}

pub enum HeaderRuleSource {
    Global,
    Run { run_id: String },
}
```

Do not store raw unresolved env references after config loading.

### Per-Run Rule Association

Because multiple agents may run concurrently, the proxy must know which per-run rules apply to each request. The simplest production-safe design for Approach 1 is to allocate a per-run proxy listener or per-run proxy token.

Recommended design: per-run proxy token.

1. Master starts one proxy listener, for example `127.0.0.1:18080`.
2. Before spawning an agent, master registers a `ProxyRunContext`:

```rust
pub struct ProxyRunContext {
    pub run_id: String,
    pub slot_id: String,
    pub rules: Vec<ResolvedHeaderOverrideRule>,
}
```

3. Master generates an unguessable per-run proxy token, stores it in an in-memory map, and sets proxy URLs with embedded proxy credentials:

```text
HTTP_PROXY=http://run:<token>@127.0.0.1:18080
HTTPS_PROXY=http://run:<token>@127.0.0.1:18080
ALL_PROXY=http://run:<token>@127.0.0.1:18080
NO_PROXY=127.0.0.1,localhost,::1
```

4. Proxy extracts `Proxy-Authorization: Basic ...`, resolves the token to the run context, applies global plus per-run rules, then strips `Proxy-Authorization` before forwarding.
5. Master unregisters the token after the agent exits or times out.

This avoids binding a new proxy port per run, avoids port exhaustion, and keeps per-run state out of agent-visible files. The token is not an auth credential for external APIs; it only identifies the run to the local proxy. It should still be random and short-lived.

Alternative: per-run listener ports. This is simpler to reason about but has higher operational cost and more port lifecycle edge cases. Prefer token-based context routing.

### Proxy Server Behavior

The proxy must handle:

- Plain HTTP proxy requests with absolute-form URIs, e.g. `GET http://example.com/path HTTP/1.1`.
- HTTPS `CONNECT host:443 HTTP/1.1` tunneling.

For phase 1:

- Inject headers only for plaintext HTTP requests that the proxy can parse and forward.
- For HTTPS `CONNECT`, tunnel bytes unchanged and record a request event containing scheme, host, and port only. Log that header injection was skipped for encrypted tunnel traffic.

If HTTPS header injection is required, add a phase 2 MITM design with CA generation and trust injection.

Recommended dependencies:

```toml
hyper = { version = "1", features = ["client", "http1", "server"] }
hyper-util = { version = "0.1", features = ["client", "client-legacy", "http1", "server", "tokio"] }
http-body-util = "0.1"
regex = "1"
globset = "0.4"
base64 = "0.22"
rand = "0.8"
```

Use `tokio::net::TcpListener` for the proxy listener and `hyper`/`hyper-util` for request parsing and forwarding. Use `tokio::io::copy_bidirectional` for `CONNECT` tunnels.

### Header Injection Rules

For each proxied HTTP request:

1. Authenticate proxy token and find run context.
2. Build normalized target URL.
3. Start with original request headers.
4. Apply matching global rules in config order.
5. Apply matching per-run rules in request order.
6. Remove hop-by-hop headers before forwarding:
   - `Connection`
   - `Proxy-Connection`
   - `Keep-Alive`
   - `Proxy-Authenticate`
   - `Proxy-Authorization`
   - `TE`
   - `Trailer`
   - `Transfer-Encoding` when body framing is rebuilt by hyper
   - `Upgrade`
7. Forward request to the target server.
8. Stream response back to agent.

Validation:

- Reject invalid header names using `http::HeaderName` parsing.
- Reject invalid header values using `http::HeaderValue` parsing.
- Reject CR/LF in header values.
- Enforce max counts and lengths:
  - Max per-run rules: 128.
  - Max global rules: 1024.
  - Max endpoint pattern length: 2048 bytes.
  - Max header key length: 256 bytes.
  - Max header value length: 8192 bytes.
- Reject attempts to override hop-by-hop headers.
- Consider rejecting `Host` override in phase 1 unless there is a concrete use case.

## Master Integration

### Startup

Modify `src/main.rs` serve path:

1. Parse `--proxy-config` / `PI_SANDBOX_PROXY_CONFIG`.
2. Load global proxy config.
3. Start proxy server before accepting run requests.
4. Pass a cloneable `ProxyHandle` into `Master::new(...)` or a new `Master::with_proxy(...)` constructor.

Suggested structure:

```rust
let proxy = ProxyHandle::start(proxy_config).await?;
let master = Master::new(root, slots, isolation, Some(proxy)).await?;
```

If no proxy config is present and proxying is still desired for per-run `inject_headers`, start the proxy with an empty global config. Because `/api/run` can now include per-run rules, the proxy should be enabled by default unless explicitly disabled.

Recommended CLI:

```text
--proxy-listen 127.0.0.1:18080
--proxy-config /var/lib/pi-sandbox/state/proxy-headers.json
--disable-proxy
```

### Run Lifecycle

Modify `Master::run_agent_in_slot` in `src/master/mod.rs`:

1. Validate `req.inject_headers` in `validate_request`.
2. Resolve per-run rules into typed matchers/header values.
3. Register run context with proxy, receiving proxy env vars.
4. Add proxy env vars to the clean env before spawning the sandbox.
5. Spawn and supervise agent as today.
6. Always unregister run context in cleanup, including spawn failure and timeout paths.

Use a guard type to ensure unregister happens even on errors:

```rust
struct ProxyRunGuard { ... }

impl Drop for ProxyRunGuard {
    fn drop(&mut self) { ... }
}
```

Because unregister may be async, either use synchronous map removal inside `Drop` or explicitly call async unregister in all paths and keep `Drop` as best-effort fallback.

### Environment Injection

Add proxy env vars after `clean_env_for_slot(...)` and before `SandboxLaunchConfig` is built. Do not allow request-provided env vars to override proxy settings.

Update `DENY_EXACT` in `src/policy/defaults.rs` to include:

```text
HTTP_PROXY
HTTPS_PROXY
ALL_PROXY
NO_PROXY
http_proxy
https_proxy
all_proxy
no_proxy
SSL_CERT_FILE
REQUESTS_CA_BUNDLE
NODE_EXTRA_CA_CERTS
GIT_SSL_CAINFO
```

Even if phase 1 does not implement MITM, reserve TLS-related env keys so request payloads cannot interfere with a future trusted CA injection path.

## Isolation Mode Compatibility

### Privileged Mode

- Works because the proxy listener is in the shared container network namespace and the agent can connect to `127.0.0.1:<proxy_port>`.
- The synthetic rootfs does not block loopback networking.
- No rootfs changes are required for phase 1 because proxy env vars are enough.
- If phase 2 MITM is added, CA certificate injection must account for `src/sandbox/rootfs.rs` bind-mounting `/etc/ssl` and `/etc/ca-certificates` read-only.

### User Mode

- Works because user-mode agents also share the container network namespace.
- No mount namespace or rootfs changes are required.
- Proxy env vars point to `127.0.0.1:<proxy_port>` and work from the per-slot UID.
- The global config remains inaccessible if stored in `/var/lib/pi-sandbox/state` with `0700` parent directory and `0600` file mode.

## Security Considerations

- This feature keeps external API auth values in the master process and proxy config only.
- Per-run header values are supplied by the `/api/run` caller and are not secret from that caller. They are still not injected into the agent environment.
- Global env var resolution must happen only in the master. Resolved values must not be logged.
- The proxy token is visible to the agent through `HTTP_PROXY`; it must only grant access to that run's proxy context and must expire immediately after the run.
- The proxy must strip `Proxy-Authorization` before forwarding.
- The proxy must reject hop-by-hop header overrides.
- The proxy must not allow request smuggling via malformed absolute URIs, conflicting `Host` headers, or CR/LF header values.
- The proxy should log metadata only by default: run ID, slot ID, method, normalized URL with optional query redaction, matched rule count, status, duration. Do not log injected header values.
- Add an optional debug mode for request headers only if explicitly enabled, and redact configured sensitive header names by default.

## Observability

Add tracing events:

- Proxy startup: listen addr, global rule count.
- Run context registered/unregistered: run ID, slot ID, per-run rule count.
- Request proxied: run ID, method, target host/path, matched global count, matched per-run count, response status, duration.
- Header injection skipped for HTTPS `CONNECT` in phase 1.
- Rule validation failures with rule index and reason, but never with secret values.

Optional future `/api/run` event:

```rust
AgentRunEvent::ProxyRequest { ... }
```

Do not add this in phase 1 unless the UI/client needs streaming proxy visibility. Tracing logs are enough initially.

## Tests

### Unit Tests

- `HeaderOverrideRule` deserialization defaults and validation.
- Glob matching:
  - `https://google.com/*` matches `https://google.com/search`.
  - Does not match `https://evilgoogle.com/search`.
- Regex matching with `regex:` prefix.
- Rule precedence: per-run overrides global for same header.
- Env resolution:
  - `env:NAME` resolves.
  - `${NAME}` resolves.
  - Missing env fails.
  - Literal values remain literal.
- Permission validation for global config on Unix.
- Header validation rejects invalid names, invalid values, hop-by-hop overrides, excessive lengths.

### Integration Tests

- Start proxy with one global rule and one per-run rule.
- Run an agent command using `curl` or a small Rust/Node/Python HTTP client against a local test HTTP server.
- Assert the upstream test server receives injected headers.
- Assert per-run rule overrides global rule for the same header.
- Assert after run completion the proxy token is removed and cannot be reused.
- Run the same test under `PI_SANDBOX_ISOLATION=privileged`.
- Run the same test under `PI_SANDBOX_ISOLATION=user`.
- Verify request-supplied `HTTP_PROXY` in `env` cannot override master proxy env.

### Manual Verification

Example global config:

```json
{
  "global_inject_headers": [
    {
      "endpoint": "http://host.docker.internal:9000/*",
      "override_header_key": "Authorization",
      "override_header_value": "env:UPSTREAM_TOKEN"
    }
  ]
}
```

Example `/api/run` request:

```json
{
  "run_id": "proxy-smoke",
  "command": ["curl", "-sS", "http://host.docker.internal:9000/api/test"],
  "inject_headers": [
    {
      "endpoint": "http://host.docker.internal:9000/api/*",
      "override_header_key": "X-Run-Header",
      "override_header_value": "run-value"
    }
  ]
}
```

Expected upstream headers:

```text
Authorization: <value from UPSTREAM_TOKEN>
X-Run-Header: run-value
```

## Implementation Phases

### Phase 1: Plain HTTP Header Injection

1. Add request schema types and validation.
2. Add global config loader with permission checks and env resolution.
3. Add matcher module with glob and regex support.
4. Add proxy server for plaintext HTTP forward proxy requests.
5. Add proxy token/context registry.
6. Integrate proxy env vars into run lifecycle.
7. Add unit and integration tests for HTTP.
8. Update docs and example payloads.

### Phase 2: HTTPS Options Decision

Choose one of:

- `CONNECT` tunnel only: proxy can audit host/port but cannot inject HTTPS headers.
- Full MITM: proxy can inject HTTPS headers but requires CA generation and trust injection.

If full MITM is selected:

1. Generate a master CA on startup or first use.
2. Store private key `0600` under `/var/lib/pi-sandbox/state`.
3. Inject CA trust into both modes:
   - Privileged: rootfs CA bundle integration.
   - User: workspace file plus `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, `NODE_EXTRA_CA_CERTS`, `GIT_SSL_CAINFO`.
4. Add explicit opt-in config because MITM changes TLS trust behavior.

### Phase 3: Hard Enforcement Optional

If bypass prevention becomes mandatory, implement Approach 3 later:

- Add `CAP_NET_ADMIN`.
- Install `iptables` or use nftables.
- Redirect traffic by agent UID to a transparent proxy.
- Keep the same header-rule engine and global/per-run config model.

## Open Questions

1. Do you need header injection into HTTPS requests in phase 1, or is plaintext HTTP injection plus HTTPS `CONNECT` tunneling acceptable initially?
2. Should per-run `inject_headers` be allowed to override global rules, or should global rules always win for selected headers like `Authorization`?
3. Should endpoint matching include query strings, or should matching stop at path only?
4. Should the proxy redact query strings in logs by default?
5. Is JSON acceptable for the global config file, or do you prefer TOML once the existing `--config` placeholder becomes real?

## Recommended Defaults

- Enable proxy automatically when either global rules exist or `inject_headers` is non-empty.
- Use one shared proxy listener: `127.0.0.1:18080`.
- Use per-run proxy tokens to associate requests with run-specific rules.
- Permit per-run rules to override global rules by default.
- Match endpoint including path and query, with query redacted in logs.
- Implement plaintext HTTP header injection first; require explicit confirmation before adding HTTPS MITM.
