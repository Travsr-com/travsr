# travsr-opencode

OpenCode plugin that makes the agent use the Travsr code graph without being told to.

On load it finds the `travsr` binary (installing it with the signed shell installer if absent),
indexes the repository if it is a git repo with no index yet, and registers the Travsr MCP
server with Code Mode off so each tool is visible to the model by name.

On every model request it:

- hides the maintenance tools an agent never needs (`synonym_*`, `repos_*`, logs, health, stats),
- adds a short playbook to the system prompt saying which tool answers which question,
- resolves code names the user typed (`snake_case`, `camelCase`, `Type::path`, backticked) to
  their exact definitions and puts those in front of the prompt. Plain prose gets no lookup.

## Load it

```json
{ "plugins": [{ "package": "/path/to/packages/travsr-opencode", "options": { "binary": "/path/to/travsr" } }] }
```

`binary` is optional; without it the plugin uses `travsr` on PATH, then `~/.local/bin/travsr`.

An existing `travsr` MCP entry is kept as written, except that Code Mode is turned off unless the
entry sets `codemode` itself. Disable the server with `"disabled": true` under `mcp.servers.travsr`
(OpenCode 2 format); the older top-level `mcp.travsr` form loses `disabled` in OpenCode's v1
compatibility path. When the travsr tools are not visible to the model, the playbook is left out.

## See what it did

Add `"log": "/tmp/travsr-opencode.log"` to `options`. One line per user turn and per tool call:

```
<time> turn ses_… tools:visible injected: charge_card: function fn:charge_card at payments.py:1
<time> tool ses_… travsr_get_callers completed 126ms {"symbol":"charge_card"}
```

`tools:NOT VISIBLE` means the model has no travsr tools this turn (server disabled, or Code Mode
forced on). `grep` or `read` lines for a question the graph answers are the misses to look at.

## Why each route in the playbook

Measured over MCP stdio on the travsr repo itself (18k nodes), one run each, 2026-11-07.

| Question | Tool | Result |
|---|---|---|
| Where is X defined | `search_symbol(exact)` | exact hit, 123 ms |
| Who calls X | `get_callers` | correct set, 126 ms; tests are listed alongside production callers |
| Every use of X | `find_references` | 8 path:line sites, 151 ms |
| Read X | `get_snippets` | 3-line body, ~55 tokens, 25 ms |
| What F imports | `get_dependencies` | 25 ms |
| What breaks if F changes | `get_blast_radius` | 153 ms |
| How A reaches B | `get_execution_path` | path found, 139 ms, 18.5k chars |
| Literal text | `find_pattern` | unscoped: 116k chars (matches in untracked bench JSON); hence "always pass scope" |
| Concept, as a sentence | `get_context` / `search_symbol` | 0/2 found the answer (abstained) |
| Concept, as 2-3 code words | `search_symbol` | 2/2 found the answer |
| Repo / subsystem overview | `get_architecture_brief` / `get_subsystem_brief` | 157 ms / 36 ms |

The concept rows are why the playbook tells the model to turn a question into code words before
searching: the model translates the query, the graph answers it.
