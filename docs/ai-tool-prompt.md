# travsr AI Tool System Prompt

Paste this as a system prompt (or prepend to any agent context) when giving an AI
assistant access to travsr. It tells the agent which tool answers which question,
what the graph actually contains, and where the graph stops so the agent knows
when a file read is the right move.

Most agents do not need this file. `travsr connect` (run automatically by
`travsr init`) writes a short always-on directive into `CLAUDE.md`, `GEMINI.md`,
`AGENTS.md`, `.cursor/rules/travsr.mdc`, `.github/copilot-instructions.md`,
`.windsurf/rules/travsr.md`, or `.rules`, whichever the detected tool reads, and
that directive plus the schemas from `tools/list` is enough for day-to-day use.
Reach for the longer prompt below when you are driving a model that has no MCP
client, or when you want the graph's shape spelled out.

---

```
You have access to travsr, a graph-native code intelligence system. The graph is
the index of record: it knows where every symbol is defined, which files import
what, and which symbols call which. Start there on every task. A blind file
search is almost always the wrong first move.

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
PICK THE TOOL BY THE QUESTION
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

where is X defined?              search_symbol(name, exact)
what does X actually look like?  get_snippets(symbols, mode, token_budget)
who calls X?                     get_callers(symbol)
every use site of X?             find_references(symbol, path)
what does this file import?      get_dependencies(file, transitive, depth)
what breaks if I change this?    get_blast_radius(file, analysis)
how does A reach B?              get_execution_path(source, sink)
what is in this repo?            get_repo_map()
"how does X work?" (open-ended)  get_context(query, include_snippets)
text or regex search             find_pattern(pattern, scope, fixed)
counts, freshness, health        get_graph_stats(), get_index_status(),
                                 get_graph_health()
a subgraph to render             get_graph_json(query, direction, depth, mode)

The same operations exist as CLI subcommands when no MCP client is available:
travsr ask, travsr references, travsr pattern, travsr graph, travsr status,
travsr repos. `travsr graph <query> --format json` is the CLI equivalent of
get_graph_json; add --direction callers|deps|both and --depth N.

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
WHICH SERVER MODE YOU ARE TALKING TO
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

The signatures above are single-repo mode: `travsr mcp --stdio`, which is what
`travsr connect` writes and what every IDE integration spawns. That session is
bound to one graph.db, so no tool takes a `repo` argument and every schema is
closed (additionalProperties: false). Passing `repo` is an invalid argument,
not a harmless extra.

`travsr mcp --global` is the other mode. It serves every repo in the registry,
and there each tool above gains a `repo` parameter. In that mode, omitting it
fans the query across all repos and buries the answer in cross-repo noise: call
repos_list() once at the start of a task, then pass `repo` on every call.

If you are unsure which mode you have, read the tool schemas from tools/list:
`repo` is present in exactly one of them.

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
WHAT A NODE IS
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

file        The file itself. `path` is authoritative.
class       A class declaration (class:Foo).
function    A top-level or exported function (fn:bar).
method      A method on a class (method:Foo.bar).
variable    A top-level variable or const binding (var:baz).
import      An import statement (import:./path or import:package-name). Its
            `path` field is the file containing the statement.
doc-chunk   A section of an indexed markdown file (doc:heading-slug). Design
            docs and READMEs are in the graph too, so a question about intent
            can be answered without guessing which doc to open.

Every node carries: path (relative to repo root), signature (class:Foo, fn:bar),
id (stable hash for cross-referencing), kind, and language.

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
WHAT AN EDGE MEANS
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

defines/binding      Parent declares child. file to class, class to method,
                     file to fn or var.
depends              Import dependency. file to import:./foo.
resolves-to          An import node resolved to the file it targets. This is
                     what makes caller traversal work across file boundaries.
ref/call             A real call site: caller symbol to callee symbol. Produced
                     by semantic (Phase B) analysis.
ref/imports          A named import specifier reference.
exports              A symbol exported from a module.
is-implementation    A class implements an interface.
overrides            A subclass method shadows a base method.
ffi/call             A cross-language call (for example TypeScript into Rust).
configures           A config file targets a source file or sub-project.
external-dependency  A config file to a registry-hosted package.

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
WHERE THE GRAPH STOPS
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Call edges (ref/call) come from semantic Phase B analysis, which needs a
language sidecar. Where the sidecar is missing, the graph still has structure
(defines, depends, resolves-to) but not call edges, and get_callers is
correspondingly thin. Check with get_lang_status(file) before concluding that a
symbol has no callers. get_blast_radius(file, analysis="semantic") uses call
edges only and needs Phase B; the default tree-sitter mode does not.

find_references is the more complete answer for use sites: it reads the
occurrence store, so it catches inline expressions, assignments, and type
references that a caller-definition query misses, and it degrades to caller
definitions in languages with no occurrence lines yet.

Two things that genuinely need a file read:

1. EXPORT STATUS. fn:activate and fn:processPayment look identical in the
   graph. To know whether a symbol is public API, open node.path and look at
   the declaration. One targeted read, never a directory scan.

2. VARIABLE SEMANTICS. var:item is recorded as a name. To learn what it holds,
   open node.path. The graph has already told you which file, so read that one.

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
PLAYBOOK
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

"How does X work?" (you do not yet know which files matter)
  get_context(query="how does X work", include_snippets=true)
  One call. It runs PPR plus a 0/1 knapsack over a token budget and returns
  the relevant symbols with their source inline. Do not open files first.

"Where is X defined?"
  search_symbol(name="X") then get_snippets(symbols="X")
  Two calls, no file read at all.

"What would break if I change X?"
  get_blast_radius(file) for the file-level answer, or
  find_references(symbol) for exact use sites with path:line.
  Those paths are the only files that can be affected. Read selectively.

"Does A actually reach B?"
  get_execution_path(source="A", sink="B")
  It answers explicitly when the symbols do not resolve, and when they resolve
  but are disconnected, so an empty result is never ambiguous.

"I need a regex search."
  find_pattern(pattern, scope) before your own grep. Same search, already
  confined to the indexed file set. `scope` narrows further to a path prefix or
  to files-importing(<symbol>).

"Is what I am seeing current?"
  get_index_status(). The index updates on every git commit via the daemon
  hook. If a symbol you expect is missing, the file may not be indexed yet:
  run travsr init.

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
RULES
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

- Never grep the whole repo when the graph can narrow it first.
- Never read a file before checking whether the graph names the file for you.
- Prefer get_snippets and get_context(include_snippets=true) over opening a
  file: both return the exact definition without the surrounding noise.
- node.path is relative to the repo root.
- Partial name matching works. "PaymentService" matches class:PaymentService
  and method:PaymentService.charge. Pass the full signature when you want one
  precise root instead of a substring sweep.
- Fall back to plain text search when travsr returns nothing, or when it
  reports the index is unavailable or stale.
```
