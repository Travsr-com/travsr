# RFC-005: MCP Tool JSON Schema Contracts

**Status:** Accepted
**Author:** Travsr Engineering
**Date:** 2026-05-18
**Crate(s) affected:** travsr-mcp

## Summary

Define and enforce JSON Schema contracts for all MCP tool inputs exposed via
`tools/list`. Every tool must declare an `inputSchema` that MCP clients can use
for codegen, validation, and IDE autocomplete. A `_schemaVersion` field enables
non-breaking evolution of the contract.

## Motivation

- The MCP spec requires `inputSchema` per tool in `tools/list`.
- Missing schemas break MCP client codegen (TypeScript SDK, Python SDK).
- The Phase 3 VS Code extension needs structured payloads for type-safe calls.
- Without schemas, client-side validation is impossible and errors surface only
  at runtime rather than at development time.

## Detailed Design

### Schema versioning

A top-level `_schemaVersion` field in the `tools/list` result carries a semver
string (e.g. `"1.0.0"`). The versioning policy:

| Change type | Version bump |
|---|---|
| Remove or rename a required field | Major (breaking) |
| Add a new required field | Major (breaking) |
| Add a new optional field | Minor |
| Change a description string only | Patch |

### Tool schemas (v1.0.0)

| Tool | Required inputs | Optional inputs |
|---|---|---|
| `get_dependencies` | `file: string` | — |
| `get_callers` | `symbol: string` | — |
| `get_blast_radius` | `file: string` | — |
| `search_symbol` | `name: string` | — |
| `get_repo_map` | — | — |

Global-mode variants add `repo?: string` as an optional input to all tools.

All schemas use `"additionalProperties": false` to prevent undocumented arguments
from being silently ignored.

### Unimplemented tools

`get_blast_radius`, `search_symbol`, and `get_repo_map` appear in `tools/list`
with full schemas but return a graceful `"not yet implemented"` text response
rather than an RPC error when called. This allows clients to discover the full
API surface and build against it without being blocked by Phase 3 delivery.

## Alternatives Considered

- **Dynamic schema generation from Rust types** — adds build complexity via
  `schemars`. Deferred to Phase 3 when the tool implementations exist.
- **OpenAPI overlay** — overkill for 5 tools; MCP uses its own JSON-RPC schema
  wire format that doesn't map cleanly to OpenAPI paths.

## Drawbacks

- Unimplemented tools appear in `tools/list` — clients calling them receive a
  text response instead of an RPC error. This is intentional but may surprise
  clients that expect unannounced tools to be absent.

## Unresolved Questions

- `outputSchema` format is not yet standardised in the MCP spec — deferred.
- Per-tool error codes and error schema contracts — deferred to Phase 3.
