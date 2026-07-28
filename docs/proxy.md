# Outbound proxy

Agentic Sandbox starts a forward proxy by default and gives each run a random,
temporary proxy credential. Cooperative HTTP clients use the injected
`HTTP_PROXY`, `HTTPS_PROXY`, and `ALL_PROXY` environment variables. The proxy can
override headers for matching destinations and route matching requests through
another proxy.

This is not a network enforcement boundary. A child can ignore proxy environment
variables and open a direct outbound connection.

## Per-run rules

Add rules to `AgentRunRequest.inject_headers`:

```json
{
  "run_id": "proxy-example",
  "command": ["curl", "https://api.example.com/v1/items"],
  "inject_headers": [
    {
      "endpoint": "https://api.example.com/*",
      "override_header_key": "Authorization",
      "override_header_value": "Bearer run-token",
      "http_proxy": null,
      "https_proxy": "http://upstream-proxy.example:8080"
    }
  ]
}
```

`override_header_key` and `override_header_value` must appear together. Both may be
omitted or `null` for a routing-only rule. A rule with no header and no upstream
proxy is ignored.

Limits:

- at most 128 per-run rules;
- endpoint patterns at most 2048 bytes;
- header names at most 256 bytes;
- header values at most 8192 bytes;
- no CR or LF in values;
- `Host` and hop-by-hop headers cannot be overridden.

## Matching

Rules match a normalized absolute URL:

```text
<lowercase-scheme>://<lowercase-host>[:non-default-port]<path>[?query]
```

Patterns are globs by default:

```text
https://api.example.com/*
```

Prefix a pattern with `regex:` to use a regular expression:

```text
regex:^https://api\.example\.com/v[0-9]+/.+$
```

Queries participate in matching but are redacted from proxy logs.

Global rules run first and per-run rules run second. Later matching header values
replace earlier values. The last matching rule with an upstream proxy for the
request's scheme selects the route.

## Global configuration

The default file is
`/var/lib/agentic-sandbox/state/proxy-headers.json`. Supply another location with
`--proxy-config` or `AGENTIC_SANDBOX_PROXY_CONFIG`.

```json
{
  "global_inject_headers": [
    {
      "endpoint": "https://api.example.com/*",
      "override_header_key": "Authorization",
      "override_header_value": "env:EXAMPLE_API_TOKEN",
      "https_proxy": "http://upstream-proxy.example:8080"
    }
  ]
}
```

Global header values support environment lookup:

- `env:NAME`
- `${NAME}`

Missing or empty referenced variables fail startup. Per-run rule values are always
literal and do not resolve environment variables.

On Unix, the config owner must be root or the current process UID, and no group or
world permission bits may be set. Use:

```bash
chmod 0600 /var/lib/agentic-sandbox/state/proxy-headers.json
```

The global configuration limit is 1024 rules.

## Run-scoped authentication

Before launching a child, the control plane:

1. validates the run's rules;
2. generates a random 48-character token;
3. registers the token with the run and slot context;
4. puts a proxy URL containing that token in the child environment;
5. removes the registration when the run ends.

Requests without a live run token are rejected. Caller-supplied proxy variables
are removed by the environment policy so they cannot replace the run-scoped
values through `AgentRunRequest.env`.

The proxy token controls access to the run's rule context; it is not a general
outbound-network credential. A child can read its own environment and therefore
knows its own token.

## HTTP and HTTPS behavior

Plain HTTP requests are visible to the proxy, so matching headers can be modified
without additional configuration.

HTTPS uses `CONNECT`:

- By default, the proxy can match the destination host and select an upstream
  proxy, but it cannot see or modify encrypted paths and headers.
- With `--enable-proxy-mitm`, the proxy generates or loads a local CA, creates
  per-host certificates, and injects CA trust variables into the run. It can then
  apply full URL and header rules to clients that honor those trust variables.

The default CA files are:

```text
/var/lib/agentic-sandbox/state/proxy-ca.pem
/var/lib/agentic-sandbox/state/proxy-ca-key.pem
```

When both files already exist, startup verifies restrictive key permissions,
certificate/key correspondence, and a generated leaf certificate chain. Material
whose subject or CA constraints are incompatible with the generated interception
certificates is rejected before the proxy starts.

MITM changes the child's TLS trust and exposes decrypted traffic to the control
plane. Enable it only when required, protect the CA private key, and review
[Security](security.md#outbound-network-and-proxy).

## Disable the proxy

Use `--disable-proxy` or `AGENTIC_SANDBOX_DISABLE_PROXY=true`. Requests containing
`inject_headers` are rejected while proxy support is disabled.
