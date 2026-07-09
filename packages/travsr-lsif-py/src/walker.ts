/**
 * Python LSIF walker — two-pass tree-sitter emitter.
 *
 * Pass 1 (definitions): walks every .py / .pyi file and emits a resultSet +
 *   definitionResult + referenceResult for each top-level function, class, and
 *   method.  The resultSet carries a `travsr_vname` field so the Rust ingester
 *   can compute the Travsr NodeId without querying the store.
 *
 * Pass 2 (references): builds a per-file import table from
 *   `import_statement` / `import_from_statement` nodes, then walks every
 *   `call` node.  Calls that resolve to a first-party import emit a ref edge.
 *
 * Signature format (matches travsr-analysis/python phase_b_native_python):
 *   fn:funcName              — module-level function
 *   class:ClassName          — class definition
 *   method:ClassName.method  — method inside a class
 *
 * Known gaps (same as phase_b_native_python — intentional):
 *   - Method calls on local variables (user = create_user(); user.save())
 *     require type inference and are NOT emitted.
 *   - Star imports (from x import *) are skipped — cannot track names.
 *   - Dynamic imports (importlib.import_module) are skipped.
 *   - Functions defined inside if/for/try/with blocks are not indexed.
 */

import * as fs from 'fs';
import * as path from 'path';
import Parser from 'tree-sitter';
import Python from 'tree-sitter-python';
import { Emitter } from './emitter';
import { assertPathsContained, isUnderRoot, resolveRoot } from './security';

type SyntaxNode = Parser.SyntaxNode;

interface SymbolInfo {
  resultSetId: number;
  definitionResultId: number;
  referenceResultId: number;
  documentId: number;
}

// DefMap key: "{relPath}:{signature}"
// e.g. "src/utils.py:fn:helper" or "models.py:class:User"
type DefMap = Map<string, SymbolInfo>;

// Import entry: result of resolving an import statement.
type ImportEntry =
  // from x import y  →  localName maps to a specific defMap entry.
  | { kind: 'direct'; key: string }
  // import x  →  localName maps to a module; attribute calls (x.func()) are resolved.
  | { kind: 'module'; relpaths: string[] };

// Directory names that are never recursed into during file discovery.
const SKIP_DIRS = new Set([
  '.git',
  'venv',
  '.venv',
  'env',
  '__pycache__',
  'build',
  'dist',
  '.eggs',
  '.tox',
  '.mypy_cache',
  '.pytest_cache',
  'node_modules',
  '.ruff_cache',
  '.hypothesis',
  '.nox',
  'site-packages',
  '.cache',
  'eggs',
]);

// ── Public entry point ────────────────────────────────────────────────────────

export function walk(rootDir: string, emitter: Emitter): void {
  const repoRoot = resolveRoot(rootDir);

  if (!fs.existsSync(repoRoot)) {
    throw new Error(`project root does not exist: ${repoRoot}`);
  }
  if (!fs.statSync(repoRoot).isDirectory()) {
    throw new Error(`project root is not a directory: ${repoRoot}`);
  }

  const pyFiles = collectPyFiles(repoRoot);
  // SEC-003: every file must reside inside the resolved root.
  assertPathsContained(pyFiles, repoRoot);

  emitter.emitMetaData(repoRoot);
  const projectId = emitter.emitProject();

  // Emit one document vertex per file; index by repo-relative path.
  const documentIds = new Map<string, number>(); // relPath → docId
  for (const absPath of pyFiles) {
    if (!isUnderRoot(absPath, repoRoot)) continue; // belt-and-suspenders
    const relPath = toRelPath(absPath, repoRoot);
    const docId = emitter.emitDocument(absPath);
    documentIds.set(relPath, docId);
  }
  emitter.emitContains(projectId, Array.from(documentIds.values()));

  const parser = new Parser();
  parser.setLanguage(Python);

  const defMap: DefMap = new Map();

  // ── Pass 1: definitions ────────────────────────────────────────────────────
  for (const absPath of pyFiles) {
    const relPath = toRelPath(absPath, repoRoot);
    const docId = documentIds.get(relPath);
    if (docId === undefined) continue;

    const source = safeReadFile(absPath);
    if (source === null) continue;

    const tree = parser.parse(source);
    const defRangeIds: number[] = [];
    visitDefs(tree.rootNode, relPath, null, docId, defMap, emitter, defRangeIds);
    emitter.emitContains(docId, defRangeIds);
  }

  // ── Pass 2: references ─────────────────────────────────────────────────────
  for (const absPath of pyFiles) {
    const relPath = toRelPath(absPath, repoRoot);
    const docId = documentIds.get(relPath);
    if (docId === undefined) continue;

    const source = safeReadFile(absPath);
    if (source === null) continue;

    const tree = parser.parse(source);
    const refRangeIds: number[] = [];
    const fileDir = path.dirname(relPath).replace(/\\/g, '/');
    const importTable = buildImportTable(tree.rootNode, fileDir, defMap);
    // #299 P1: track local variable types (`x = SomeClass()`) so method calls on
    // locals (`x.method()`), and `self.method()`, resolve to `method:Class.method`.
    const localTypes = buildLocalTypes(tree.rootNode, importTable, defMap, relPath);
    visitRefs(
      tree.rootNode,
      docId,
      importTable,
      defMap,
      emitter,
      refRangeIds,
      relPath,
      null,
      localTypes
    );
    emitter.emitContains(docId, refRangeIds);
  }
}

// ── File discovery ─────────────────────────────────────────────────────────────

function collectPyFiles(rootDir: string): string[] {
  const results: string[] = [];
  // PY-C1: track resolved real-paths of directories we have already visited.
  // Without this, a cyclic symlink (dir → parent) causes infinite recursion
  // and eventual stack-overflow DoS on any untrusted repo.
  const visitedDirs = new Set<string>();

  function recurse(dir: string): void {
    let entries: fs.Dirent[];
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }

    for (const entry of entries) {
      if (SKIP_DIRS.has(entry.name)) continue;

      const fullPath = path.join(dir, entry.name);

      if (entry.isSymbolicLink()) {
        // PY-C1 + PY-H1: resolve to real path for both cycle-detection and I/O.
        let resolved: string;
        try {
          resolved = fs.realpathSync(fullPath);
        } catch {
          continue;
        }
        try {
          const stat = fs.statSync(resolved);
          if (stat.isDirectory() && !SKIP_DIRS.has(path.basename(resolved))) {
            // PY-C1: skip if we have already visited this real directory.
            if (visitedDirs.has(resolved)) continue;
            visitedDirs.add(resolved);
            recurse(resolved);
          } else if (stat.isFile() && isPyFile(entry.name)) {
            // PY-H1: push the realpath so reads use the validated path,
            // eliminating the TOCTOU gap between the containment check and read.
            results.push(resolved);
          }
        } catch {
          continue;
        }
      } else if (entry.isDirectory()) {
        recurse(fullPath);
      } else if (entry.isFile() && isPyFile(entry.name)) {
        results.push(fullPath);
      }
    }
  }

  recurse(rootDir);
  results.sort(); // deterministic ordering across platforms
  return results;
}

function isPyFile(name: string): boolean {
  return name.endsWith('.py') || name.endsWith('.pyi');
}

// ── Pass 1: definition visitor ─────────────────────────────────────────────────

/**
 * Visit the direct named children of `node` at module or class-body scope.
 * Functions defined inside conditionals or loops are intentionally not indexed.
 */
function visitDefs(
  node: SyntaxNode,
  relPath: string,
  enclosingClass: string | null,
  docId: number,
  defMap: DefMap,
  emitter: Emitter,
  defRangeIds: number[],
  depth = 0
): void {
  // PY-H2: guard against pathologically nested Python ASTs (e.g. deeply nested
  // class definitions in generated code) that could overflow the JS call stack.
  if (depth >= MAX_AST_DEPTH) return;
  for (const child of node.namedChildren) {
    visitDefsNode(child, relPath, enclosingClass, docId, defMap, emitter, defRangeIds, depth + 1);
  }
}

function visitDefsNode(
  node: SyntaxNode,
  relPath: string,
  enclosingClass: string | null,
  docId: number,
  defMap: DefMap,
  emitter: Emitter,
  defRangeIds: number[],
  depth = 0
): void {
  switch (node.type) {
    case 'decorated_definition': {
      // A decorator wraps a class or function — unwrap to the inner definition.
      const inner = node.lastNamedChild;
      if (inner) {
        visitDefsNode(inner, relPath, enclosingClass, docId, defMap, emitter, defRangeIds, depth);
      }
      break;
    }

    case 'class_definition': {
      const nameNode = node.childForFieldName('name');
      if (!nameNode) break;
      const className = nameNode.text;
      emitSymbol(`class:${className}`, nameNode, relPath, docId, defMap, emitter, defRangeIds);

      // Recurse into the class body at the next nesting level.
      const bodyNode = node.childForFieldName('body');
      if (bodyNode) {
        visitDefs(bodyNode, relPath, className, docId, defMap, emitter, defRangeIds, depth + 1);
      }
      break;
    }

    case 'function_definition': {
      const nameNode = node.childForFieldName('name');
      if (!nameNode) break;
      const funcName = nameNode.text;
      const signature = enclosingClass
        ? `method:${enclosingClass}.${funcName}`
        : `fn:${funcName}`;
      emitSymbol(signature, nameNode, relPath, docId, defMap, emitter, defRangeIds);
      // Do NOT recurse — nested functions are not indexed.
      break;
    }

    default:
      // All other statement types (if, for, try, with, …) at module or
      // class scope are not recursed — we only index top-level defs.
      break;
  }
}

function emitSymbol(
  signature: string,
  nameNode: SyntaxNode,
  relPath: string,
  docId: number,
  defMap: DefMap,
  emitter: Emitter,
  defRangeIds: number[]
): void {
  const key = `${relPath}:${signature}`;

  if (!defMap.has(key)) {
    const resultSetId = emitter.emitResultSet({ path: relPath, signature });
    const defResultId = emitter.emitDefinitionResult();
    const refResultId = emitter.emitReferenceResult();
    emitter.emitEdge('textDocument/definition', resultSetId, defResultId);
    emitter.emitEdge('textDocument/references', resultSetId, refResultId);
    defMap.set(key, {
      resultSetId,
      definitionResultId: defResultId,
      referenceResultId: refResultId,
      documentId: docId,
    });
  }

  const info = defMap.get(key)!;
  const rangeId = emitter.emitRange(nameNode);
  emitter.emitEdge('next', rangeId, info.resultSetId);
  emitter.emitItem(info.definitionResultId, [rangeId], docId, 'definitions');
  defRangeIds.push(rangeId);
}

// ── Import table ───────────────────────────────────────────────────────────────

/**
 * Build an import table for one file by walking its top-level import nodes.
 *
 * Handles:
 *   import foo            → module entry (foo → ["foo.py", "foo/__init__.py"])
 *   import foo.bar        → module entry (foo → ["foo/bar.py", "foo/bar/__init__.py"])
 *   import foo as alias   → module entry (alias → relpaths of foo)
 *   from x import y       → direct entry (y → defMap key in x.py or x/__init__.py)
 *   from x import y as z  → direct entry (z → defMap key for y in x)
 *   from . import y       → relative direct (y → defMap key in __init__.py of current pkg)
 *   from .x import y      → relative direct (y → defMap key in sibling module x)
 *   from x import *       → skipped (cannot track star-imported names)
 */
function buildImportTable(
  rootNode: SyntaxNode,
  fileDir: string,
  defMap: DefMap
): Map<string, ImportEntry> {
  const table = new Map<string, ImportEntry>();

  for (const child of rootNode.namedChildren) {
    if (child.type === 'import_statement') {
      for (const importedNode of child.namedChildren) {
        if (importedNode.type === 'dotted_name') {
          const modulePath = importedNode.text;
          // `import a.b.c` — only the first segment is in scope as a name.
          const localName = modulePath.split('.')[0]!;
          table.set(localName, { kind: 'module', relpaths: moduleToRelpaths(modulePath) });
        } else if (importedNode.type === 'aliased_import') {
          const nameNode = importedNode.childForFieldName('name');
          const aliasNode = importedNode.childForFieldName('alias');
          if (nameNode && aliasNode) {
            table.set(aliasNode.text, {
              kind: 'module',
              relpaths: moduleToRelpaths(nameNode.text),
            });
          }
        }
      }
    } else if (child.type === 'import_from_statement') {
      const moduleNameNode = child.childForFieldName('module_name');
      if (!moduleNameNode) continue;

      let moduleCandidates: string[];
      if (moduleNameNode.type === 'relative_import') {
        const raw = moduleNameNode.text; // e.g. ".utils" or "..models" or "."
        const level = countLeadingDots(raw);
        const modulePart = raw.slice(level);
        moduleCandidates = resolveRelativeCandidates(modulePart, fileDir, level);
      } else {
        // dotted_name: absolute import
        moduleCandidates = moduleToRelpaths(moduleNameNode.text);
      }

      const importedNames = extractImportedNames(child, moduleNameNode);
      for (const { localName, importedName } of importedNames) {
        const key = lookupInDefMap(importedName, moduleCandidates, defMap);
        if (key !== null) {
          table.set(localName, { kind: 'direct', key });
        }
      }
    }
  }

  return table;
}

function extractImportedNames(
  importFromNode: SyntaxNode,
  moduleNameNode: SyntaxNode
): Array<{ localName: string; importedName: string }> {
  const results: Array<{ localName: string; importedName: string }> = [];

  for (const child of importFromNode.namedChildren) {
    if (child === moduleNameNode) continue;
    if (child.type === 'wildcard_import') return []; // skip *

    if (child.type === 'import_list') {
      // Parenthesized list: from x import (y, z)
      for (const item of child.namedChildren) {
        const entry = extractSingleName(item);
        if (entry) results.push(entry);
      }
    } else {
      const entry = extractSingleName(child);
      if (entry) results.push(entry);
    }
  }

  return results;
}

function extractSingleName(
  node: SyntaxNode
): { localName: string; importedName: string } | null {
  if (node.type === 'dotted_name') {
    return { localName: node.text, importedName: node.text };
  }
  if (node.type === 'aliased_import') {
    const nameNode = node.childForFieldName('name');
    const aliasNode = node.childForFieldName('alias');
    if (nameNode && aliasNode) {
      return { localName: aliasNode.text, importedName: nameNode.text };
    }
    if (nameNode) {
      return { localName: nameNode.text, importedName: nameNode.text };
    }
  }
  return null;
}

/**
 * Search defMap for `name` as fn: or class: across each candidate module path.
 * Returns the first matching key, or null if not found.
 */
function lookupInDefMap(
  name: string,
  moduleCandidates: string[],
  defMap: DefMap
): string | null {
  for (const mod of moduleCandidates) {
    for (const sigPrefix of ['fn', 'class']) {
      const key = `${mod}:${sigPrefix}:${name}`;
      if (defMap.has(key)) return key;
    }
  }
  return null;
}

// ── Pass 2: reference visitor ──────────────────────────────────────────────────

/** PY-H2: maximum AST recursion depth for visitRefs / visitDefs. */
const MAX_AST_DEPTH = 2000;

/** A local variable's inferred class type: the file + class name of its def. */
interface LocalType {
  relpath: string;
  className: string;
}

function visitRefs(
  node: SyntaxNode,
  docId: number,
  importTable: Map<string, ImportEntry>,
  defMap: DefMap,
  emitter: Emitter,
  refRangeIds: number[],
  relPath: string,
  enclosingClass: string | null,
  localTypes: Map<string, LocalType>,
  depth = 0
): void {
  // PY-H2: bail out before the JS call stack overflows on deeply nested ASTs.
  if (depth >= MAX_AST_DEPTH) return;

  if (node.type === 'call') {
    const funcNode = node.childForFieldName('function');
    if (funcNode) {
      const info = resolveCallTarget(
        funcNode,
        importTable,
        defMap,
        relPath,
        enclosingClass,
        localTypes
      );
      if (info) {
        const rangeId = emitter.emitRange(funcNode);
        emitter.emitEdge('next', rangeId, info.resultSetId);
        emitter.emitItem(info.referenceResultId, [rangeId], docId, 'references');
        refRangeIds.push(rangeId);
      }
    }
  }

  // Track the enclosing class so `self.method()` resolves inside method bodies.
  const nextClass =
    node.type === 'class_definition'
      ? (node.childForFieldName('name')?.text ?? enclosingClass)
      : enclosingClass;

  for (const child of node.namedChildren) {
    visitRefs(
      child,
      docId,
      importTable,
      defMap,
      emitter,
      refRangeIds,
      relPath,
      nextClass,
      localTypes,
      depth + 1
    );
  }
}

/**
 * Scan a file for `var = SomeClass(...)` assignments and record `var`'s class
 * type when `SomeClass` resolves to a first-party class (same file or a direct
 * import). File-scoped and last-write-wins — sufficient for the common case
 * without full flow analysis.
 */
function buildLocalTypes(
  rootNode: SyntaxNode,
  importTable: Map<string, ImportEntry>,
  defMap: DefMap,
  relPath: string
): Map<string, LocalType> {
  const types = new Map<string, LocalType>();
  const visit = (node: SyntaxNode, depth = 0): void => {
    if (depth >= MAX_AST_DEPTH) return;
    if (node.type === 'assignment') {
      const left = node.childForFieldName('left');
      const right = node.childForFieldName('right');
      if (left?.type === 'identifier' && right?.type === 'call') {
        const fn = right.childForFieldName('function');
        if (fn?.type === 'identifier') {
          const cls = resolveClassName(fn.text, importTable, defMap, relPath);
          if (cls) types.set(left.text, cls);
        }
      }
    }
    for (const child of node.namedChildren) visit(child, depth + 1);
  };
  visit(rootNode);
  return types;
}

/** Resolve a class name to its defining file + name (same file or direct import). */
function resolveClassName(
  name: string,
  importTable: Map<string, ImportEntry>,
  defMap: DefMap,
  relPath: string
): LocalType | undefined {
  if (defMap.has(`${relPath}:class:${name}`)) {
    return { relpath: relPath, className: name };
  }
  const entry = importTable.get(name);
  if (entry?.kind === 'direct') {
    const m = /^(.*):class:(.+)$/.exec(entry.key);
    if (m) return { relpath: m[1], className: m[2] };
  }
  return undefined;
}

function resolveCallTarget(
  funcNode: SyntaxNode,
  importTable: Map<string, ImportEntry>,
  defMap: DefMap,
  relPath: string,
  enclosingClass: string | null,
  localTypes: Map<string, LocalType>
): SymbolInfo | undefined {
  if (funcNode.type === 'identifier') {
    // foo() — simple direct call: look up the name in the import table.
    const entry = importTable.get(funcNode.text);
    if (entry?.kind === 'direct') {
      return defMap.get(entry.key);
    }
    return undefined;
  }

  if (funcNode.type === 'attribute') {
    // x.foo() — attribute call.
    const objNode = funcNode.childForFieldName('object');
    const attrNode = funcNode.childForFieldName('attribute');
    if (!objNode || !attrNode) return undefined;

    // Only handle simple identifier objects (not chained calls like a.b.c()).
    if (objNode.type !== 'identifier') return undefined;

    const entry = importTable.get(objNode.text);
    if (entry?.kind === 'module') {
      // Module member call: look up foo inside the imported module.
      for (const relpath of entry.relpaths) {
        for (const sigPrefix of ['fn', 'class']) {
          const key = `${relpath}:${sigPrefix}:${attrNode.text}`;
          const info = defMap.get(key);
          if (info) return info;
        }
      }
      return undefined;
    }

    // #299 P1: `self.method()` inside a class body resolves to the enclosing
    // class's method.
    if (objNode.text === 'self' && enclosingClass) {
      const info = defMap.get(`${relPath}:method:${enclosingClass}.${attrNode.text}`);
      if (info) return info;
    }

    // #299 P1: `local.method()` where `local` has a tracked class type resolves
    // to that class's method.
    const lt = localTypes.get(objNode.text);
    if (lt) {
      const info = defMap.get(`${lt.relpath}:method:${lt.className}.${attrNode.text}`);
      if (info) return info;
    }

    return undefined;
  }

  return undefined;
}

// ── Module path / import resolution helpers ────────────────────────────────────

function countLeadingDots(text: string): number {
  let count = 0;
  for (const ch of text) {
    if (ch === '.') count++;
    else break;
  }
  return count;
}

/**
 * Resolve relative import candidates relative to the importing file's directory.
 *
 * level=1 ("from . import x")    → {fileDir}/__init__.py  (if no modulePart)
 * level=1 ("from .utils import") → {fileDir}/utils.py, {fileDir}/utils/__init__.py
 * level=2 ("from ..models import")→ {parent}/models.py, {parent}/models/__init__.py
 */
function resolveRelativeCandidates(
  modulePart: string,
  fileDir: string,
  level: number
): string[] {
  // Normalise fileDir: remove empty and "." segments.
  const parts = fileDir.split('/').filter((p) => p !== '' && p !== '.');
  const upCount = Math.max(0, level - 1);
  if (upCount > parts.length) return []; // cannot go above the project root

  const anchorParts = parts.slice(0, parts.length - upCount);
  const anchor = anchorParts.join('/');

  if (!modulePart) {
    // Bare dot: `from . import x` → names live in the package __init__.py.
    const prefix = anchor ? `${anchor}/` : '';
    return [`${prefix}__init__.py`];
  }

  const subPath = modulePart.replace(/\./g, '/');
  const base = anchor ? `${anchor}/${subPath}` : subPath;
  return [`${base}.py`, `${base}/__init__.py`];
}

/**
 * Convert a dotted module name to its two candidate repo-relative file paths.
 * e.g. "os.path" → ["os/path.py", "os/path/__init__.py"]
 */
function moduleToRelpaths(modulePath: string): string[] {
  const base = modulePath.replace(/\./g, '/');
  return [`${base}.py`, `${base}/__init__.py`];
}

// ── Utility helpers ────────────────────────────────────────────────────────────

function toRelPath(absPath: string, repoRoot: string): string {
  return path.relative(repoRoot, absPath).replace(/\\/g, '/');
}

/** PY-H3: maximum file size we will read. Files larger than this are skipped. */
const MAX_FILE_BYTES = 5 * 1024 * 1024; // 5 MB

function safeReadFile(absPath: string): string | null {
  try {
    // PY-H3: size cap before readFileSync to prevent OOM on huge generated files.
    const stat = fs.statSync(absPath);
    if (stat.size > MAX_FILE_BYTES) {
      return null;
    }
    return fs.readFileSync(absPath, 'utf-8');
  } catch (err) {
    // PY-M1: log per-file errors to stderr so they appear in sidecar output
    // rather than being silently swallowed. Non-fatal: continue with other files.
    process.stderr.write(`[travsr-lsif-py] skipping ${absPath}: ${String(err)}\n`);
    return null;
  }
}
