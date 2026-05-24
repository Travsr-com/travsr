# ADR-007: Website Stack — Astro + Docusaurus + Cloudflare Pages

**Date:** 2026-05-24
**Status:** Accepted
**Issue:** #178
**Phase:** 4 (Sprint 15)
**Author:** Solution Architect
**Note:** ADR-006 (rust-analyzer subprocess trust model, S9/Issue #122) was written before Phase 4 planning; this record takes the next available number. Issue #178 refers to "ADR-006" — that reference should be read as ADR-007.

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
| Analytics | **Self-hosted Plausible CE** on OCI A1 ARM64 |
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

- Free tier: unlimited requests, 500 builds/month, unlimited bandwidth.
- PR preview deployments included.
- Custom domains + automatic TLS at no cost.
- Avoids using the 20 GB OCI Object Storage quota that is reserved for binary releases.
- Cloudflare as authoritative DNS simplifies custom domain wiring (no CNAME flattening issues).

### Self-hosted Plausible CE

- No cookies → no GDPR/PECR consent banner required.
- No personal data collected or sent to third parties.
- Aligned with Travsr's local-first, privacy-first brand.
- Runs as a ~100 MB Docker container on the existing OCI A1 `travsr-mcp-server` instance alongside the MCP SSE server.

### Rejected alternatives

| Alternative | Reason rejected |
|---|---|
| Vercel / Netlify | Free tier bandwidth caps; Cloudflare Pages has no per-request cost |
| OCI Object Storage static site | No CDN, no PR previews, consumes release storage quota |
| Single unified site (one framework) | Marketing and docs have different cadences; coupling them adds merge noise |
| Google Analytics / Mixpanel | Requires cookie consent banner; conflicts with Travsr's privacy-first positioning |
| Next.js for marketing site | SSR adds hosting complexity; a landing page has no need for server rendering |

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

All DNS managed through Cloudflare (authoritative). Orange-cloud proxy must be active on both A/CNAME records.

| Record type | Name | Value | Purpose |
|---|---|---|---|
| CNAME (proxied) | `travsr.com` | `travsr-website.pages.dev` | Marketing site |
| CNAME (proxied) | `docs` | `travsr-docs.pages.dev` | Docs site |
| TXT | `travsr.com` | `MS=ms<code>` (from Microsoft) | VS Code Verified Publisher |

---

## Plausible CE — OCI A1 Deployment

Plausible CE shares the `travsr-mcp-server` OCI A1 instance (2 OCPU, 12 GB RAM).

```
nginx :443
  ├── mcp.travsr.com  → travsr-mcp:3000
  └── plausible.travsr.com → plausible:8000
```

Both the Astro site and Docusaurus site include the Plausible script snippet:

```html
<script defer data-domain="travsr.com" src="https://plausible.travsr.com/js/script.js"></script>
```

No personal data is collected. No cookie banner is required.

---

## Consequences

### Positive

- Zero hosting cost for both web properties.
- Global CDN; TTFB target < 100 ms.
- PR preview URLs from Cloudflare Pages enable async design review.
- Privacy-first analytics with no GDPR friction.
- Independent deploy pipelines reduce blast radius of a bad merge.

### Negative

- Two Cloudflare Pages projects to manage (minor operational overhead).
- Plausible requires an additional Docker container and Nginx vhost entry on OCI A1.
- Docusaurus is React-based; heavy theming customisation requires React expertise.
- Cloudflare Pages 500 builds/month cap — unlikely to be reached, but should be monitored during active content sprints.
