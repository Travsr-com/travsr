# Language sanity fixtures

One mini-repo per language, each with a **cross-file call** so Phase B has a
real edge to resolve rather than a single file that only exercises the grammar.

| Fixture | Definition | Call site |
| --- | --- | --- |
| `c/` | `add_numbers` in `math_util.c` | `main.c`, and `scale_value` in the same file |
| `cpp/` | `app::Widget::draw` in `widget.cpp` | `main.cpp`, and `build_default` in the same file |
| `typescript/` | `makeGreeter` / `Greeter` in `greeter.ts` | `main.ts` |
| `javascript/` | `addNumbers` in `util.mjs` / `util.cjs` | `main.mjs` (ESM), `legacy.cjs` (CommonJS) |

Both an in-file and a cross-file caller exist on purpose: if only the
cross-file one is asserted, a provider that resolves nothing still looks
plausible when the graph happens to contain the definition.

## `compile_commands.json` is a template, not a real compdb

`c/` and `cpp/` ship `compile_commands.json` with `__FIXTURE_DIR__` where an
absolute path belongs. scip-clang resolves `directory` + `file` against the
filesystem, so a checked-in absolute path would be correct on exactly one
machine and silently wrong everywhere else, including CI.

The harness copies the fixture into a temp repo and substitutes the real path
at run time. Nothing else should read these files directly.

## JavaScript covers module flavours, not just syntax

`.mjs` (ESM), `.cjs` (CommonJS `require`/`module.exports`), and plain `.js`
(ambient script) are separate files rather than one, because the interesting
failure is a *flavour* being skipped or mis-attributed, not a statement failing
to parse.

Note that all of these currently land in the graph as `language = "typescript"`
(`Language::from_extension`, `travsr-core/src/lib.rs`), while `travsr lang list`
reports `javascript` as its own active language. The tests assert today's
behaviour and name the disagreement.
