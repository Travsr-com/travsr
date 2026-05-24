# RFC-007: MCP SSE Transport

**Status:** Accepted
**Author:** Travsr Engineering
**Date:** 2026-05-24
**Issue:** #184
**Crate(s) affected:** `travsr-mcp`, `travsr-cli`
**Must merge before:** S16-2 implementation starts (#186)

---

## Summary

Define the wire protocol for MCP over Server-Sent Events (SSE). This RFC locks all decisions that `travsr-mcp/src/sse.rs` and `travsr-mcp/src/auth.rs` must implement. The stdio transport (RFC-002) remains unchanged for local-first deployments. SSE is additive — cloud tier only.

---

## Motivation

The local daemon communicates with IDE clients via stdio, which requires the server to be co-located with the client. Phase 4's Cloud Preview (S16) must expose MCP to remote IDE clients and the VS Code extension over HTTPS. Server-Sent Events over HTTP/1.1 are the correct primitive:

- Unidirectional server→client streaming maps cleanly to MCP's response model
- HTTP infrastructure (Nginx, load balancers, CDNs) handles SSE well
- No upgrade handshake (unlike WebSockets) reduces auth surface
- All existing MCP tool handlers are unchanged — SSE is a transport shim only

---

## Detailed Design

### 1. URL Layout

```
GET  /sse   — opens SSE stream; bearer token in Authorization header
POST /rpc   — sends JSON-RPC 2.0 request; bearer token in Authorization header
GET  /health — unauthenticated; returns {"status":"ok","tenants":N}
GET  /metrics — unauthenticated; Prometheus text format
```

**Rationale for split endpoints:** A single bidirectional stream would require the client to write to the same connection it reads from, complicating proxy buffering configuration. Separate `GET /sse` + `POST /rpc` endpoints map to standard HTTP semantics, allow independent rate-limiting, and give Nginx a clean split: `proxy_buffering off` only on `/sse`.

Responses to `POST /rpc` are delivered on the client's open `GET /sse` stream, correlated by `id` field in the SSE event matching the JSON-RPC `id`. Clients must open the SSE stream before issuing any `/rpc` requests.

### 2. Message Framing

Each SSE event carries exactly one JSON-RPC 2.0 object:

```
id: <event-id>
data: {"jsonrpc":"2.0","id":"req-1","result":{...}}

```

Multi-line JSON is not split across `data:` continuation lines — the entire JSON object is serialized as a single line (no embedded newlines). This simplifies client parsers and avoids ambiguity in the SSE spec's continuation semantics.

Event IDs are monotonically increasing integers per tenant connection, formatted as decimal strings (`"1"`, `"2"`, …). They are not globally unique — they are scoped per-tenant per-connection.

Notification events (server-initiated, no `id` in JSON-RPC) use event type `notification`:

```
event: notification
id: <event-id>
data: {"jsonrpc":"2.0","method":"travsr/indexUpdated","params":{...}}

```

Keepalive heartbeats use SSE comment syntax (no `id`, no `data`):

```
: keepalive

```

### 3. Heartbeat and Reconnect

**Heartbeat interval:** The server emits `: keepalive` every **15 seconds** on every open SSE stream. This prevents intermediate proxies and load balancers from closing idle connections.

**Client reconnect timeout:** Clients must reconnect if no event (including keepalive) is received within **30 seconds**. The SSE `retry:` field is set to `15000` (milliseconds) on stream open to configure compliant SSE clients automatically.

**Reconnect with `Last-Event-ID`:** On reconnect, the client sends the `Last-Event-ID` header with the last received event ID. The server replays all events after that ID from the in-memory ring buffer.

```
Ring buffer spec:
  - Capacity: 1000 events per tenant connection
  - TTL: events older than 5 minutes are evicted
  - On capacity overflow: oldest event evicted (FIFO)
  - On reconnect with unknown Last-Event-ID: stream begins fresh (no replay)
  - On reconnect with Last-Event-ID "0" or absent: stream begins fresh
```

Each event is delivered **exactly once** per connection. Replay on reconnect covers only events the client provably missed (ID gap), not redelivery of already-acknowledged events.

### 4. Authentication

**Token format:** Bearer tokens are HMAC-SHA256 MACs over the payload `{tenant_id}:{timestamp_unix_seconds}`, encoded as `base64url(payload) + "." + base64url(hmac)`.

```
payload  = base64url("{tenant_id}:{unix_ts}")
mac      = HMAC-SHA256(signing_key, payload)
token    = payload + "." + base64url(mac)
```

Timestamps must be within ±60 seconds of server time. Replay outside this window is rejected with `401 Unauthorized`.

**Token delivery:** The `Authorization: Bearer <token>` header on both `GET /sse` and every `POST /rpc`. Tokens are not accepted as query parameters (prevents accidental logging in Nginx access logs).

**Signing key:** A 256-bit random key stored in OCI Vault. Identifier `travsr-mcp-signing-key`. The server fetches it at startup and caches in memory.

**Key rotation:** 30-day rotation schedule. The old key remains valid for **24 hours** after a new key is activated (grace period covers clients that cached the old token). The server must accept tokens signed by either the current key or the one previous key during the grace period. OCI Vault stores both; the server loads both on startup. After 24 hours the old key is deleted from Vault.

**Tenant ID extraction:** `tenant_id` is the authenticated identity extracted from the bearer token payload. It must match the directory name under `/data/tenants/` on disk — the supervisor enforces this mapping. Mismatches are rejected with `403 Forbidden`.

### 5. Multi-Tenancy

**Isolation level:** Process-per-tenant. Each authenticated tenant gets a dedicated child process spawned by the supervisor with:
- `TRAVSR_DATA_DIR=/data/tenants/<tenant_id>/`
- No shared memory with other tenant processes
- Independent SQLite connection (WAL mode)

**Beta cap:** 25 concurrent tenant processes maximum. The 26th distinct tenant request returns `503 Service Unavailable` with body `{"error":"beta_capacity_reached"}`.

**`Last-Event-ID` namespace:** Event IDs are scoped per-tenant per-connection. A client that somehow obtains another tenant's event ID cannot replay that tenant's stream — the server validates that the `Last-Event-ID` belongs to the requesting tenant's connection.

**Tenant ID format:** Lowercase alphanumeric + hyphens, max 64 characters. Validated at token issuance time. Rejected tokens with path-traversal characters (`.`, `/`, `\`) with `400 Bad Request`.

### 6. Error Model

| HTTP Status | Meaning |
|---|---|
| `400` | Malformed request (bad JSON, path-traversal in tenant ID) |
| `401` | Missing or invalid bearer token (expired, bad HMAC, timestamp drift) |
| `403` | Token valid but tenant_id mismatch |
| `503` | Beta capacity reached (25 tenant cap) |
| `200` | SSE stream open (`Content-Type: text/event-stream`) |

JSON-RPC errors in the tool layer are returned as `200` with a JSON-RPC error object in the SSE stream, not as HTTP error codes.

### 7. Nginx Configuration (normative)

The following Nginx directives are **required** for correct SSE behavior. Deviating from these will break reconnect or keepalive:

```nginx
location /sse {
    proxy_pass http://localhost:3000;
    proxy_http_version 1.1;
    proxy_set_header Connection '';        # SSE requires persistent connection
    proxy_buffering off;                   # MUST be off — buffering kills SSE
    proxy_cache off;
    proxy_read_timeout 86400s;            # long-lived SSE connections
    chunked_transfer_encoding on;
}

location /rpc {
    proxy_pass http://localhost:3000;
    proxy_http_version 1.1;
    limit_req zone=rpc burst=20 nodelay;   # 10 req/s per tenant
}
```

---

## Alternatives Considered

| Option | Why Rejected |
|---|---|
| WebSockets | More complex handshake, harder to proxy, no proven benefit over SSE for MCP's unidirectional response model |
| HTTP long-polling | Higher latency than SSE; each response requires a new connection; poor fit for streaming notifications |
| stdio tunnel over SSH | Requires SSH access to the server; no path to web-based IDE clients |
| Bidirectional SSE (write to same stream) | Breaks standard SSE semantics; complicates `proxy_buffering off` scope |

---

## Security Considerations

- Tokens are not logged. Nginx `log_format` must exclude `$http_authorization`. The server never logs the raw token.
- Timestamp binding prevents replay attacks beyond the ±60s window.
- The 24-hour grace period for key rotation is an accepted trade-off: it reduces key rotation operational risk while keeping the replay window short.
- Tenant ID path-traversal validation is enforced at token issuance time AND at supervisor fork time — defense in depth.
- `/health` and `/metrics` are unauthenticated but serve only aggregate counts, no tenant data.

---

## Implementation Notes

- `travsr-mcp/src/sse.rs` — axum HTTP server; implements `GET /sse`, `POST /rpc`, ring buffer, keepalive task
- `travsr-mcp/src/auth.rs` — bearer token verification per this RFC; OCI Vault fetch on startup
- `travsr-daemon/src/multi_tenant.rs` — supervisor; forks per-tenant; enforces 25-process cap
- `travsr-cli` — new subcommand `serve --sse --port 3000 --tenants-dir /data/tenants`
- `axum` chosen over `actix` and `hyper` for ergonomics and tokio-native integration (S16 decision)

---

## Definition of Done

- This RFC merged to `docs/rfcs/`
- All questions in issue #184 resolved and documented above
- S16-2 (#186) implementation references this RFC as the source of truth
