# ADR-008: Website Stack — Astro + Docusaurus + Cloudflare Pages

**Date:** 2026-05-24
**Status:** Accepted
**Issue:** #178
**Phase:** 4 (Sprint 15)
**Author:** Solution Architect
**Component:** web-infrastructure
**Note:** ADR-006 (rust-analyzer subprocess trust model, S9/Issue #122) and ADR-007 (PCST lambda selection, S13) were written before Phase 4 planning locked numbering. Issue #178 refers to "ADR-006" — that reference should be read as ADR-008.

---

## Context

S15 ships two web properties:

- `travsr.com` — marketing site (landing page, tagline, waitlist form)
- `docs.travsr.com` — technical documentation (getting started, MCP tools, architecture)

Requirements:

1. **Zero hosting cost** — OCI Object Storage (20 GB free) must be preserved for binary releases, not used for web assets.
2. **Global CDN, sub-100 ms TTFB** — both sites are read-heavy and static.
3. **PR preview deployments** — designers and content reviewers need a live URL per PR.
4. **Privacy-first analytics** — no Google Analytics; no cookie consent banner.
5. **Independent deploy pipelines** — marketing copy and docs have different change cadences; one site update must not block the other.

---

## Decision

| Property | Choice |
|---|---|
| Marketing site | **Astro 4.x** (`website/`) |
| Docs site | **Docusaurus 3.4** (`docs-site/`) |
| Hosting | **Cloudflare Pages** (free tier) |
| Analytics | **Self-hosted Plausible CE** on OCI A1 ARM64 (`travsr-indexer` instance) |
| DNS authority | **Cloudflare** (authoritative nameservers) |

### Astro for `travsr.com`

- Static-first: zero JS shipped to browser by default.
- Excellent Lighthouse scores out of the box.
- Island architecture allows opt-in interactivity (e.g. waitlist form) without a full React bundle.
- Simple enough for a landing page; not over-engineered.

### Docusaurus for `docs.travsr.com`

- Versioned docs with built-in sidebar navigation.
- MDX support for interactive code examples.
- Algolia DocSearch integration path for later sprints.
- Widely adopted in OSS (React, Jest, etc.); community ecosystem is large.

### Cloudflare Pages

- Free tier: unlimited requests, 500 builds/month (account-wide across all projects), unlimited bandwidth.
- PR preview deployments included; CF Pages scopes build triggers to each project's root directory so a docs-only change does not trigger a marketing-site rebuild.
- Custom domains + automatic TLS at no cost when the DNS record is proxied (orange-cloud) through Cloudflare.
- Avoids using the 20 GB OCI Object Storage quota that is reserved for binary releases.
- Cloudflare as authoritative DNS simplifies custom domain wiring (no CNAME flattening issues at zone apex).

**Build budget:** The 500 builds/month limit is shared across the entire Cloudflare account (both CF Pages projects combined). Set a Cloudflare email notification at 400 builds/month consumed to avoid exhausting the quota during active content sprints.

### Self-hosted Plausible CE

- No cookies → no GDPR/PECR consent banner required.
- No personal data collected or sent to third parties.
- Aligned with Travsr's local-first, privacy-first brand.
- Deployed on the `travsr-indexer` OCI A1 instance (2 OCPU, 12 GB RAM, 100 GB block volume) — **not** the MCP server instance — to isolate analytics workload from the latency-sensitive MCP SSE service and to use the instance with more available storage.
- Plausible CE v2.x publishes official multi-arch images (`ghcr.io/plausible/analytics`, supporting `linux/arm64`), satisfying the project ARM64 requirement. ClickHouse and Postgres also publish official `linux/arm64` images.

### Rejected alternatives

| Alternative | Reason rejected |
|---|---|
| Vercel / Netlify | Free tier bandwidth caps; Cloudflare Pages has no per-request cost |
| OCI Object Storage static site | No CDN, no PR previews, consumes release storage quota |
| Single unified site (one framework) | Marketing and docs have different cadences; coupling them adds merge noise |
| Google Analytics / Mixpanel | Requires cookie consent banner; conflicts with Travsr's privacy-first positioning |
| Next.js for marketing site | SSR adds hosting complexity; a landing page has no need for server rendering |
| Plausible Cloud | $9/mo recurring cost — violates zero-hosting-cost requirement |

---

## Directory Layout

```
travsr/
├── website/                   # travsr.com — Astro 4.x
│   ├── package.json
│   ├── astro.config.mjs
│   └── src/
│       ├── layouts/
│       │   └── Layout.astro
│       └── pages/
│           └── index.astro
└── docs-site/                 # docs.travsr.com — Docusaurus 3.4
    ├── package.json
    ├── docusaurus.config.js
    ├── sidebars.js
    ├── docs/
    │   └── intro.md
    └── src/
        └── css/
            └── custom.css
```

---

## Cloudflare Pages Projects

Two separate Cloudflare Pages projects, both connected to `raj-rkv/travsr`:

| CF Pages project | Root directory | Build command | Output directory | Domain |
|---|---|---|---|---|
| `travsr-website` | `website/` | `npm run build` | `dist/` | `travsr.com` |
| `travsr-docs` | `docs-site/` | `npm run build` | `build/` | `docs.travsr.com` |

CF Pages' "root directory" setting scopes both build triggers and preview deployments to their respective source directories.

---

## DNS Records

All DNS managed through Cloudflare (authoritative). The orange-cloud proxy **must** be active on all web-property CNAME records — Cloudflare only issues TLS certificates and provides CDN for proxied records.

| Record type | Name | Value | Proxy status | Purpose |
|---|---|---|---|---|
| CNAME | `travsr.com` | `travsr-website.pages.dev` | Proxied (orange cloud) | Marketing site |
| CNAME | `docs` | `travsr-docs.pages.dev` | Proxied (orange cloud) | Docs site |
| A | `plausible` | `<travsr-indexer public IP>` | Proxied (orange cloud) | Analytics |
| TXT | `travsr.com` | `MS=ms<code>` (from Microsoft) | DNS only | VS Code Verified Publisher |

---

## Plausible CE — OCI A1 Deployment

Plausible CE runs on the `travsr-indexer` OCI A1 instance (2 OCPU, 12 GB RAM, 100 GB block volume mounted at `/data/index`). This isolates analytics from the MCP SSE server and uses the instance with greater available storage.

### Nginx routing (per instance)

```
travsr-mcp-server (existing):
  nginx :443
    └── mcp.travsr.com → travsr-mcp:3000

travsr-indexer (new):
  nginx :443
    └── plausible.travsr.com → plausible:8000
```

### Docker Compose

```yaml
# /data/index/plausible/docker-compose.yml
services:
  plausible:
    image: ghcr.io/plausible/analytics:v2
    platform: linux/arm64
    restart: unless-stopped
    mem_limit: 1g
    depends_on: [clickhouse, postgres]
    ports:
      - "127.0.0.1:8000:8000"
    environment:
      - SECRET_KEY_BASE=${PLAUSIBLE_SECRET_KEY}
      - BASE_URL=https://plausible.travsr.com
      - DATABASE_URL=postgres://plausible:${POSTGRES_PASSWORD}@postgres:5432/plausible
      - CLICKHOUSE_DATABASE_URL=http://plausible:${CLICKHOUSE_PASSWORD}@clickhouse:8123/plausible

  clickhouse:
    image: clickhouse/clickhouse-server:24-alpine
    platform: linux/arm64
    restart: unless-stopped
    mem_limit: 4g
    environment:
      - CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=0
      - CLICKHOUSE_LISTEN_HOST=clickhouse    # bind within Docker network only; no direct host exposure
    volumes:
      - /data/index/clickhouse:/var/lib/clickhouse
      - ./clickhouse-retention.xml:/etc/clickhouse-server/config.d/retention.xml

  postgres:
    image: postgres:16-alpine
    platform: linux/arm64
    restart: unless-stopped
    mem_limit: 512m
    environment:
      - POSTGRES_DB=plausible
      - POSTGRES_USER=plausible
      - POSTGRES_PASSWORD=${POSTGRES_PASSWORD}
    volumes:
      - /data/index/postgres:/var/lib/postgresql/data
```

### ClickHouse data retention

ClickHouse data is capped to 90 days via a TTL merge tree policy, preventing unbounded disk growth:

```xml
<!-- clickhouse-retention.xml -->
<clickhouse>
  <profiles>
    <default>
      <max_memory_usage>3221225472</max_memory_usage>  <!-- 3 GB hard cap -->
    </default>
  </profiles>
</clickhouse>
```

The Plausible `events_v2` table is created with `TTL toDate(timestamp) + INTERVAL 90 DAY DELETE` on first schema migration.

### Storage budget

| Service | Data path | Allocation |
|---|---|---|
| ClickHouse (90-day TTL) | `/data/index/clickhouse` | ≤ 20 GB |
| Postgres | `/data/index/postgres` | ≤ 2 GB |
| Existing indexer data | `/data/index/lsif`, `/data/index/hash-store` | ≤ 78 GB |

Total: ≤ 100 GB — within the 100 GB block volume allocation.

### Nginx vhost

The `plausible.travsr.com` vhost inherits the same security baseline as `mcp.travsr.com`: TLS 1.3 only, Mozilla Modern cipher suite, 1-year HSTS, `X-Frame-Options`, `X-Content-Type-Options`, `server_tokens off`.

```nginx
server {
    listen 443 ssl;
    server_name plausible.travsr.com;

    ssl_certificate /etc/letsencrypt/live/plausible.travsr.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/plausible.travsr.com/privkey.pem;
    ssl_protocols TLSv1.3;
    ssl_prefer_server_ciphers off;

    add_header Strict-Transport-Security "max-age=31536000; includeSubDomains" always;
    add_header X-Frame-Options "SAMEORIGIN" always;
    add_header X-Content-Type-Options "nosniff" always;
    server_tokens off;

    location / {
        proxy_pass http://127.0.0.1:8000;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    }
}
```

### Analytics snippet

Both the Astro site and Docusaurus site include the Plausible script:

```html
<script defer data-domain="travsr.com" src="https://plausible.travsr.com/js/script.js"></script>
```

`defer` ensures the script does not block page rendering if the analytics server is momentarily unavailable. No personal data is collected. No cookie banner is required.

---

## Consequences

### Positive

- Zero hosting cost for both web properties.
- Global CDN; TTFB target < 100 ms.
- PR preview URLs from Cloudflare Pages enable async design review.
- Privacy-first analytics with no GDPR friction.
- Independent deploy pipelines reduce blast radius of a bad merge.
- Plausible on `travsr-indexer` keeps the MCP SSE server instance (travsr-mcp-server) free of analytics workload.

### Negative

- Two Cloudflare Pages projects to manage (minor operational overhead); the 500 builds/month account-wide limit requires monitoring during active content sprints.
- Plausible CE adds operational overhead on the indexer instance: Docker Compose management, Certbot renewal for `plausible.travsr.com`, and ClickHouse monitoring.
- Docusaurus is React-based; heavy theming customisation requires React expertise.
- The analytics script loads from a self-hosted OCI endpoint — if `travsr-indexer` is unreachable, the `defer`-loaded script fails silently (no user-visible impact, but analytics will have gaps during outages).
