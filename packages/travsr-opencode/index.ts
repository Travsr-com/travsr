import { Plugin } from "@opencode/plugin"
import type { Message } from "@opencode/ai"
import { execFile } from "node:child_process"
import { appendFileSync } from "node:fs"
import { homedir } from "node:os"
import { join } from "node:path"
import { promisify } from "node:util"

const run = promisify(execFile)

// Where install.sh puts the binary when TRAVSR_INSTALL_DIR is unset.
const INSTALLED = join(homedir(), ".local", "bin", process.platform === "win32" ? "travsr.exe" : "travsr")

// Names in a prompt that we resolve per turn. More is noise the model has to read past.
const MAX_SYMBOLS = 3

// Maintenance and renderer tools. An agent answering code questions never needs them, and each
// schema costs tokens on every step.
const HIDDEN = [
  "synonym_list", "synonym_add", "synonym_set", "synonym_remove", "synonym_remove_term", "synonym_reset",
  "repos_list", "repos_prune", "repos_remove",
  "get_graph_json", "get_daemon_logs", "get_graph_health", "get_graph_stats", "repo_languages", "get_lang_status",
]

// Which tool answers which question. Stable text, so the provider caches it with the system prompt.
const PLAYBOOK = `# Code navigation: use the travsr tools first
This repository is indexed by Travsr, a code graph. For any question about where code is or how it connects, call a travsr tool before grep, glob or reading whole files.
- Where is X defined: travsr_search_symbol(name, exact=true)
- Who calls X: travsr_get_callers(symbol)
- Every use of X: travsr_find_references(symbol)
- Read X's source: travsr_get_snippets(symbols="a,b")
- What does file F import: travsr_get_dependencies(file)
- What breaks if file F changes: travsr_get_blast_radius(file); for one function use travsr_get_callers instead
- How does A reach B: travsr_get_execution_path(source, sink)
- Literal text, error messages, config keys: travsr_find_pattern(pattern, fixed=true, scope="<dir>"); always pass scope, unscoped results can be huge
- Concept questions ("how does auth work"): turn the question into 2-3 words likely to appear in code names and call travsr_search_symbol with those words, then travsr_get_snippets on what it returns. Never pass a full sentence: it misses.
- How the repo is organised: travsr_get_architecture_brief; one subsystem: travsr_get_subsystem_brief
- Results look stale or empty: travsr_get_index_status
Symbol tools need the exact name; if one reports no such symbol, find the name with travsr_search_symbol first.`

export default Plugin.define({
  id: "travsr",
  async setup(ctx) {
    const cwd = ctx.location.directory
    const binary = ctx.options.binary ?? (await locate())
    if (!binary) {
      console.warn("travsr: binary not found; install with `npm i -g travsr` or set the `binary` option")
      return
    }
    // Never index from a plugin: `init` installs git hooks, writes ignore files and starts a
    // daemon. An unindexed repo (or no repo) leaves OpenCode as if the plugin were absent.
    if (!(await succeeds(binary, ["status"], cwd))) {
      console.warn("travsr: this directory has no index; run `travsr init` to enable the graph")
      return
    }

    // Optional trace of what the plugin added and which tools the model called, one line each.
    const logFile: string | undefined = ctx.options.log
    const log = (line: string) => {
      if (logFile) appendFileSync(logFile, `${new Date().toISOString()} ${line}\n`)
    }

    // Code Mode would fold every travsr tool into OpenCode's one `execute` tool, and the model
    // would never see the tools the playbook names. An entry the user wrote (or `travsr connect`
    // wrote) is kept, but gets Code Mode off unless it sets Code Mode itself.
    await ctx.mcp.transform((editor) => {
      if (!editor.get("travsr")) {
        editor.set("travsr", { type: "local", command: [binary, "mcp", "--stdio"], codemode: false })
        return
      }
      editor.update("travsr", (config) => {
        config.codemode ??= false
      })
    })

    if (logFile) {
      const started = new Map<string, number>()
      await ctx.tool.hook("execute.before", (event) => {
        started.set(event.id, Date.now())
      })
      await ctx.tool.hook("execute.after", (event) => {
        const ms = Date.now() - (started.get(event.id) ?? Date.now())
        started.delete(event.id)
        log(`tool ${event.sessionID} ${event.tool} ${event.status} ${ms}ms ${JSON.stringify(event.input).slice(0, 200)}`)
      })
    }

    // OpenCode registers MCP tools a moment after the first prompt in a directory arrives (about
    // 100 ms, measured), so that prompt's first step had no travsr tools and the model grepped.
    // Wait for them once, at most 5 s: with the server disabled there are none to wait for.
    let waited = false
    await ctx.session.hook("prompt", async () => {
      for (let i = 0; !waited && i < 50; i++) {
        if ((await ctx.tool.list()).some((t) => t.id.startsWith("travsr_"))) break
        await new Promise((r) => setTimeout(r, 100))
      }
      waited = true
    })

    // The context hook runs on every step of a turn, not once per user message. Resolve the
    // prompt's names once and splice the same text at the same place each step, so the cached
    // prefix holds.
    const resolved = new Map<string, { prompt: string; context: Promise<string | undefined> }>()

    await ctx.session.hook("context", async (event) => {
      for (const name of HIDDEN) delete event.tools[`travsr_${name}`]
      // The playbook names travsr tools; with MCP disabled or Code Mode on there are none to call.
      const visible = Object.keys(event.tools).some((name) => name.startsWith("travsr_"))
      if (visible) event.system.push({ type: "text", text: PLAYBOOK })

      const at = event.messages.findLastIndex((m) => m.role === "user")
      if (at < 0) return
      const prompt = event.messages[at].content
        .flatMap((part) => (part.type === "text" ? [part.text] : []))
        .join("\n")
      let entry = resolved.get(event.sessionID)
      const fresh = entry?.prompt !== prompt
      if (fresh) {
        entry = { prompt, context: definitions(binary, cwd, symbols(prompt)) }
        resolved.set(event.sessionID, entry)
      }
      const text = await entry!.context
      if (fresh) {
        const found = text?.split("\n").slice(2, -1).join("; ") ?? "none"
        log(`turn ${event.sessionID} tools:${visible ? "visible" : "NOT VISIBLE"} injected: ${found}`)
      }
      if (!text) return
      // Before the prompt, where OpenCode's own plan-mode reminder lands.
      event.messages.splice(at, 0, { role: "user", content: [{ type: "text", text }] } as Message)
    })
  },
})

// The binary on PATH, else where install.sh puts it. Never installs: that is the user's call.
async function locate() {
  for (const candidate of ["travsr", INSTALLED]) {
    if (await succeeds(candidate, ["--version"], process.cwd())) return candidate
  }
  return undefined
}

async function succeeds(file: string, args: string[], cwd: string) {
  try {
    await run(file, args, { cwd, timeout: 10_000 })
    return true
  } catch {
    return false
  }
}

// Identifiers the user wrote: backticked names, and words that only code spells this way
// (snake_case, camelCase, Type::path). Plain English never matches, so prose gets no lookup.
function symbols(prompt: string) {
  const found = new Set<string>()
  for (const [, name] of prompt.matchAll(/`([A-Za-z_][\w:.]*)`/g)) found.add(name)
  for (const [word] of prompt.matchAll(/[A-Za-z_][\w]*(?:(?:::|\.)[A-Za-z_]\w*)*/g)) {
    if (/[a-z0-9]_[a-zA-Z]|[a-z][A-Z]|::/.test(word)) found.add(word)
  }
  return [...found].slice(0, MAX_SYMBOLS)
}

// Exact definitions only, from `references`: a graph lookup that never loads the embedding or
// rerank model, unlike `ask`, so a prompt costs no model run. The graph spells paths with `.`.
async function definitions(binary: string, cwd: string, names: string[]) {
  const lines = await Promise.all(
    names.map(async (name) => {
      try {
        const query = name.replaceAll("::", ".")
        const { stdout } = await run(binary, ["references", query, "--format", "json"], { cwd, timeout: 10_000 })
        type Def = { path: string; line: number; kind: string; signature: string }
        const out: { resolved_to: Def | null; candidates: Def[] } = JSON.parse(stdout)
        const defs = out.resolved_to ? [out.resolved_to] : out.candidates.slice(0, MAX_SYMBOLS)
        return defs.map((d) => `${clean(name)}: ${clean(d.kind)} ${clean(d.signature)} at ${clean(d.path)}:${d.line}`)
      } catch {
        return []
      }
    }),
  )
  if (!lines.flat().length) return undefined
  return ["<travsr-data>", "Definitions of names in this request, from the code graph:", ...lines.flat(), "</travsr-data>"].join("\n")
}

// `references --format json` is not sanitised (only MCP responses are), and a path or signature
// comes from the repo, so a file named `</travsr-data>...` could close the envelope early. Same
// rule as sanitize_for_mcp: drop control characters, escape the tag characters.
function clean(text: string) {
  return text
    .replace(/[\u0000-\u001f\u007f]/g, "")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
}
