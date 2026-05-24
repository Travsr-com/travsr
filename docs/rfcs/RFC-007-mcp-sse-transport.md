# RFC-007: MCP SSE Transport

**Status:** Accepted
**Author:** Travsr Engineering
**Date:** 2026-05-24
**Issue:** #184
**Crate(s) affected:** `travsr-mcp`, `travsr-cli`, `travsr-daemon`
**Must merge before:** S16-2 implementation starts (#186)

---

## Summary

Define the wire protocol for MCP over Server-Sent Events (SSE). This RFC locks all decisions that `travsr-mcp/src/sse.rs` and `travsr-mcp/src/auth.rs` must implement. The stdio transport (RFC-002) remains unchanged for local-first deployments. SSE is additive — cloud tier only.

Token issuance (who mints bearer tokens, via what endpoint, with what credentials) is explicitly out of scope for this RFC and is deferred to RFC-008 (linked to #197). `auth.rs` implements verification only; issuance is undefined until RFC-008 is accepted.

**Dependency direction:** `travsr-daemon` imports from `travsr-mcp` (supervisor calls the MCP process). `travsr-mcp` must never import from `travsr-daemon`. This direction matches CLAUDE.md's module boundary rules and must not be reversed.

---

## Motivation

The local daemon communicates with IDE clients via stdio, which requires the server to be co-located with the client. Phase 4's Cloud Preview (S16) must expose MCP to remote IDE clients and the VS Code extension over HTTPS. Server-Sent Events over HTTP/1.1 are the correct primitive:

- Unidirectional server→client streaming maps cleanly to MCP's response model
- HTTP infrastructure (Nginx, load balancers, CDNs) handles SSE well
- No upgrade handshake (unlike WebSockets) reduces auth surface
- All existing MCP tool handlers are unchanged — SSE is a transport shim only

**Why `travsr serve` not `travsr mcp --sse`:** The existing `travsr mcp --stdio` subcommand is scoped to a single local process talking to one IDE client. The cloud SSE server is a different operational mode — it binds to a TCP port, manages multiple tenant connections, requires a tenants directory, and is never invoked interactively. Conflating the two under `mcp` would force confusing mutual-exclusion flags (`--stdio` vs `--sse`) on a subcommand whose existing users expect only stdio. A separate `serve` verb makes the operational split explicit.

---

## Detailed Design

### 1. URL Layout

```
GET  /sse     — opens SSE stream; bearer token in Authorization header
POST /rpc     — sends JSON-RPC 2.0 request; bearer token in Authorization header
GET  /health  — unauthenticated; returns {"status":"ok"}
GET  /metrics — authenticated (scope=metrics bearer token); Prometheus text format
```

**`/rpc` ordering requirement:** The server must return `503 Service Unavailable` with `Retry-After: 1` to any `POST /rpc` whose `tenant_id` has no currently-open SSE stream. Clients must open the `GET /sse` stream before issuing `/rpc` requests. This is server-enforced, not merely documented.

**`/health`:** Returns `{"status":"ok"}` only — no tenant count or capacity indicators. Sufficient for liveness probes; does not expose capacity information.

**`/metrics`:** Authenticated with a dedicated `scope=metrics` bearer token. Exposes aggregate metrics only — no per-tenant labels. `travsr_tenant_db_size_bytes` is emitted as a single aggregate sum, not per-tenant. Per-tenant labels would expose which tenants exist and their relative codebase sizes (T7 cross-tenant data leak).

**Rationale for split endpoints:** A single bidirectional stream would require the client to write to the same connection it reads from, complicating proxy buffering configuration. Separate `GET /sse` + `POST /rpc` allow independent rate-limiting and give Nginx a clean split: `proxy_buffering off` only on `/sse`.

Responses to `POST /rpc` are delivered on the client's open `GET /sse` stream, correlated by the JSON-RPC `id` field. Clients must open the SSE stream before issuing any `/rpc` requests.

### 2. Message Framing

Each SSE event carries exactly one JSON-RPC 2.0 object:

```
id: <event-id>
data: {"jsonrpc":"2.0","id":"req-uuid4","result":{...}}

```

Multi-line JSON is not split across `data:` continuation lines — the entire JSON object is serialized as a single line (no embedded newlines). This simplifies client parsers.

**JSON-RPC `id` format:** Clients must use UUID4 strings as `id` values (e.g., `"f47ac10b-58cc-4372-a567-0e02b2c3d479"`). The server must reject a `POST /rpc` with `400 Bad Request` if a request `id` is already in-flight from the same tenant. This eliminates correlation ambiguity for concurrent requests.

**Max frame size:** A single SSE `data:` line must not exceed **512 KB**. Responses exceeding this limit must be chunked using a multi-part envelope:

```json
{"jsonrpc":"2.0","id":"<uuid>","result":{"partial":true,"part":1,"total":3,"data":{...}}}
```

The client reassembles parts ordered by `part` index, keyed by `id`. This covers large `get_context` responses.

**Event IDs** are monotonically increasing integers per `session_id` (see §5), formatted as decimal strings. They are not globally unique.

Notification events (server-initiated) use event type `notification`:

```
event: notification
id: <event-id>
data: {"jsonrpc":"2.0","method":"travsr/indexUpdated","params":{...}}

```

Keepalive heartbeats use SSE comment syntax:

```
: keepalive

```

### 3. Heartbeat and Reconnect

**Heartbeat interval:** `: keepalive` every **15 seconds** on every open SSE stream.

**Client reconnect timeout:** Clients reconnect if no event is received within **30 seconds**. `retry: 15000` is set on stream open.

**Reconnect with `Last-Event-ID`:** On reconnect the client sends the `Last-Event-ID` header. The server replays all events after that ID from the in-memory ring buffer scoped to the `(tenant_id, session_id)` pair (see §5).

```
Ring buffer spec:
  - Capacity: 1000 events per (tenant_id, session_id) pair
  - TTL: events older than 5 minutes are evicted
  - On capacity overflow: oldest event evicted (FIFO)
  - On reconnect with unknown Last-Event-ID: stream begins fresh (no replay)
  - On reconnect with Last-Event-ID "0" or absent: stream begins fresh
```

Each event is delivered **exactly once** per connection. Replay covers only events the client provably missed (ID gap).

### 4. Authentication

**Token payload encoding:** The payload is `base64url(tenant_id) + ":" + unix_ts_decimal`. The `tenant_id` component is itself base64url-encoded before payload construction, making `:` structurally impossible in that field regardless of the raw tenant ID content. This eliminates injection via a tenant ID containing `:`.

```
tenant_b64  = base64url(tenant_id_bytes)
payload     = tenant_b64 + ":" + str(unix_seconds)
mac         = HMAC-SHA256(signing_key, payload_bytes)
token       = payload + "." + base64url(mac)
```

**Token scopes:** The payload includes a `scope` field before the timestamp separator:

```
tenant_b64 + ":" + scope + ":" + unix_ts
```

Valid scopes: `mcp` (all MCP tools via `/sse` and `/rpc`), `metrics` (read-only `/metrics`). Future admin or narrow-scope endpoints can reuse the same format without a version bump.

**Token delivery:** `Authorization: Bearer <token>` header on `GET /sse`, every `POST /rpc`, and authenticated `GET /metrics`. Tokens are never accepted as query parameters (prevents logging in Nginx `$request_uri`).

**Timestamp window:** Tokens are valid for ±60 seconds of server time. Replay outside this window is rejected with `401 Unauthorized`.

**Signing key:** 256-bit random key stored in OCI Vault, identifier `travsr-mcp-signing-key`. The server fetches it at startup using `reqwest` against the OCI Instance Metadata Service (IMDS) endpoint `http://169.254.169.254/opc/v2/identity/` with an instance-principal auth header — no OCI SDK dependency required. The OCID of the secret is passed via the `TRAVSR_VAULT_SECRET_OCID` environment variable.

**Key rotation:** 30-day rotation schedule. The old key remains valid for **24 hours** after a new key is activated. The server loads up to two keys on startup: `travsr-mcp-signing-key` (current) and `travsr-mcp-signing-key-prev` (previous, if present in Vault). After 24 hours the previous key is deleted from Vault.

**Tenant ID format:** Lowercase alphanumeric + hyphens, max 64 characters. Validated at token issuance time (RFC-008) and re-validated at the supervisor fork point in `multi_tenant.rs`. Path-traversal characters (`.`, `/`, `\`, `:`, null byte) in the raw tenant ID cause `400 Bad Request` at both points — defense in depth.

### 5. Connection Identity and Ring Buffer Keying

A **connection** is identified by a server-assigned `session_id` (UUID4), delivered as the first SSE event on stream open:

```
event: session
id: 0
data: {"session_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479"}

```

The ring buffer is keyed by `(tenant_id, session_id)`. `Last-Event-ID` replay is validated against this pair — a client cannot replay another tenant's events even if it guesses a valid event ID, because the `session_id` is server-assigned and bound to the authenticated tenant at stream open.

On reconnect, the client includes both `Last-Event-ID` and the `X-Session-ID: <session_id>` header. The server uses both to locate the correct ring buffer partition.

### 6. Multi-Tenancy

**Isolation level:** Process-per-tenant. Each authenticated tenant gets a dedicated child process spawned by `travsr-daemon/src/multi_tenant.rs` with:
- `TRAVSR_DATA_DIR=/data/tenants/<tenant_id>/`
- No shared memory with other tenant processes
- Independent SQLite connection (WAL mode)

**Beta cap:** 25 concurrent tenant processes maximum. The 26th distinct tenant request returns `503 Service Unavailable` with body `{"error":"beta_capacity_reached"}`.

**`Last-Event-ID` namespace:** Event IDs are scoped per `(tenant_id, session_id)`. Cross-tenant replay is structurally impossible because the ring buffer lookup requires both components and the `session_id` is server-assigned at authenticated stream open.

### 7. Error Model

| HTTP Status | Meaning |
|---|---|
| `400` | Malformed request (bad JSON, path-traversal in tenant ID, duplicate in-flight `id`) |
| `401` | Missing or invalid bearer token (expired, bad HMAC, timestamp drift) |
| `403` | Token valid but scope insufficient or tenant_id mismatch |
| `503` | Beta capacity reached (25-tenant cap) or `/rpc` with no open SSE stream |
| `200` | SSE stream open (`Content-Type: text/event-stream`) |

JSON-RPC errors in the tool layer are returned as `200` with a JSON-RPC error object in the SSE stream, not as HTTP error codes.

### 8. Nginx Configuration (normative)

All directives below are **required**. Deviating from them will break SSE, allow header logging, or weaken TLS.

```nginx
# TLS hardening — Mozilla SSL Config "modern" profile
server {
    listen 443 ssl;
    server_name mcp.travsr.com;

    ssl_certificate     /etc/letsencrypt/live/mcp.travsr.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/mcp.travsr.com/privkey.pem;
    ssl_protocols       TLSv1.3;
    ssl_prefer_server_ciphers off;

    add_header Strict-Transport-Security "max-age=31536000; includeSubDomains; preload" always;
    add_header X-Content-Type-Options nosniff always;
    add_header X-Frame-Options DENY always;

    # Log format — Authorization header must never appear in logs
    access_log /var/log/nginx/travsr_access.log travsr_safe;

    location /sse {
        proxy_pass http://localhost:3000;
        proxy_http_version 1.1;
        proxy_set_header Connection '';
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_buffering off;
        proxy_cache off;
        proxy_read_timeout 86400s;
        chunked_transfer_encoding on;
        add_header X-Accel-Buffering no;          # prevents CDN re-buffering
        limit_conn sse_conn 5;                    # max 5 SSE streams per IP
    }

    location /rpc {
        proxy_pass http://localhost:3000;
        proxy_http_version 1.1;
        limit_req zone=rpc burst=20 nodelay;      # 10 req/s per IP
    }

    location /health {
        proxy_pass http://localhost:3000;
    }

    location /metrics {
        proxy_pass http://localhost:3000;
        # Authentication enforced in travsr-mcp; no IP restriction needed
    }
}

# Log format — Authorization header excluded
log_format travsr_safe '$remote_addr - $remote_user [$time_local] '
                       '"$request" $status $body_bytes_sent '
                       '"$http_referer" "$http_user_agent"';
# Note: $http_authorization is intentionally omitted from this format.

# HTTP → HTTPS redirect
server {
    listen 80;
    server_name mcp.travsr.com;
    return 301 https://$host$request_uri;
}

# Connection limiter zone (declared in http block)
limit_conn_zone $binary_remote_addr zone=sse_conn:10m;
limit_req_zone  $binary_remote_addr zone=rpc:10m rate=10r/s;
```

---

## Alternatives Considered

| Option | Why Rejected |
|---|---|
| WebSockets | More complex handshake, harder to proxy, no proven benefit over SSE for MCP's unidirectional response model |
| HTTP long-polling | Higher latency than SSE; each response requires a new connection; poor fit for streaming notifications |
| stdio tunnel over SSH | Requires SSH access to the server; no path to web-based IDE clients |
| Bidirectional SSE (write to same stream) | Breaks standard SSE semantics; complicates `proxy_buffering off` scope |
| `travsr mcp --sse` (extend existing subcommand) | `mcp --stdio` is single-client, interactive, no port binding; `serve --sse` is multi-tenant, daemon-mode, requires `--tenants-dir`; conflating them forces confusing mutual-exclusion flags |

---

## Security Considerations

- Tokens are never logged. The normative `log_format travsr_safe` in §8 explicitly omits `$http_authorization`. Nginx access logs must use this format on all travsr locations.
- Payload double-encoding (`base64url(tenant_id)`) makes `:` in the tenant field structurally impossible, eliminating payload injection attacks regardless of tenant ID content.
- `/metrics` is authenticated and exposes only aggregate values — no per-tenant labels that would enumerate tenant identities or relative codebase sizes.
- The 24-hour grace period for key rotation is an accepted trade-off: it reduces key rotation operational risk while keeping the replay window bounded. Must be reassessed before GA once token revocation (RFC-008) is defined.
- Rate limiting on `/sse` (`limit_conn 5`) and `/rpc` (`10r/s`) prevents a single IP from exhausting the 25-tenant beta cap via connection flooding.
- Token issuance is deferred to RFC-008. Until RFC-008 is accepted, tokens must be provisioned manually via OCI Vault CLI by an operator — no automated issuance path exists.

---

## Implementation Notes

- `travsr-mcp/src/sse.rs` — axum HTTP server; `GET /sse`, `POST /rpc`, ring buffer keyed on `(tenant_id, session_id)`, keepalive task, `session_id` assignment on stream open
- `travsr-mcp/src/auth.rs` — bearer token verification: base64url-decode tenant, validate scope, HMAC-SHA256 check, timestamp window; OCI Vault key fetch via `reqwest` + IMDS; two-key support during rotation grace period
- `travsr-daemon/src/multi_tenant.rs` — supervisor; forks per-tenant child processes; enforces 25-process cap; tenant ID path-traversal re-validation at fork time
- `travsr-cli` — new top-level subcommand `serve --sse --port 3000 --tenants-dir /data/tenants`; distinct from `mcp --stdio` (see motivation section)
- Ring buffer: `VecDeque<(u64, Instant, String)>` with manual TTL eviction — no external crate needed
- `axum` added to `travsr-mcp/Cargo.toml`; `reqwest` (already in workspace or to be added) used for OCI Vault IMDS fetch

---

## Definition of Done

- This RFC merged to `docs/rfcs/`
- All questions from issue #184 resolved above
- Token issuance gap explicitly acknowledged and linked to follow-on RFC-008 (#197)
- S16-1 (#185) and S16-2 (#186) reference this RFC as the source of truth
