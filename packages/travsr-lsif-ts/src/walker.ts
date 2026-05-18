/**
 * TypeScript compiler API walker — two-pass LSIF emitter.
 *
 * Pass 1  (definitions): walks every project source file and emits a
 *   resultSet + definitionResult + referenceResult for each named declaration
 *   (class, interface, function, method, variable). Ranges at definition sites
 *   are linked via `next` and `item/definitions`.
 *
 * Pass 2  (references): re-walks every file and emits:
 *   - RefCall          — call expressions resolved to a project declaration
 *   - RefImports       — named import specifiers resolved to a project declaration
 *   - IsImplementation — `implements` clauses resolved to a project interface
 *   - Overrides        — method declarations that shadow a base-class method
 *
 * The two-pass design is necessary because a reference in file A can point to
 * a declaration in file B that hasn't been visited yet in a single pass.
 *
 * Visitor functions (`visitDef`, `visitRef`) are defined at module level and
 * receive all mutable state through a context object so no new closure is
 * allocated per source file iteration.
 */

import * as ts from 'typescript';
import * as path from 'path';
import { Emitter } from './emitter';
import { assertFilesContained, isUnderRoot, resolveRoot, sanitizeTsconfig } from './security';

interface SymbolInfo {
  resultSetId: number;
  definitionResultId: number;
  referenceResultId: number;
  /** Set lazily when the first `implements` clause targets this symbol. */
  implementationResultId?: number;
  /** Document vertex ID of the file that contains the primary declaration. */
  documentId: number;
}

interface DefCtx {
  sf: ts.SourceFile;
  docId: number;
  defRangeIds: number[];
  symbolInfos: Map<ts.Symbol, SymbolInfo>;
  checker: ts.TypeChecker;
  emitter: Emitter;
  /** Absolute path to the tsconfig directory — used to compute repo-relative VName paths. */
  basePath: string;
}

interface RefCtx {
  sf: ts.SourceFile;
  docId: number;
  refRangeIds: number[];
  symbolInfos: Map<ts.Symbol, SymbolInfo>;
  checker: ts.TypeChecker;
  emitter: Emitter;
}

export function walk(tsconfigPath: string, emitter: Emitter): void {
  const configFile = ts.readConfigFile(tsconfigPath, ts.sys.readFile);
  if (configFile.error) {
    throw new Error(
      `tsconfig parse error: ${ts.flattenDiagnosticMessageText(configFile.error.messageText, '\n')}`
    );
  }

  const basePath = path.dirname(tsconfigPath);

  // SEC-003 — Check 1: reject plugins / escaping extends / escaping references
  // before handing the config to the TS compiler. Hard error, no fallback.
  sanitizeTsconfig(configFile.config, basePath);

  const parsed = ts.parseJsonConfigFileContent(configFile.config, ts.sys, basePath);

  // SEC-003 — Check 2: every resolved file must be inside the project root.
  // Uses realpathSync to follow symlinks. Catches malicious globs and files[].
  const repoRoot = resolveRoot(basePath);
  assertFilesContained(parsed.fileNames, repoRoot);

  const fileSet = new Set(parsed.fileNames.map((f) => path.normalize(f)));

  const program = ts.createProgram(parsed.fileNames, parsed.options);
  const checker = program.getTypeChecker();

  // SEC-003 — Check 3: belt-and-suspenders filter so a future refactor cannot
  // bypass checks 1 & 2 and accidentally emit ranges for out-of-root files.
  const projectFiles = program
    .getSourceFiles()
    .filter(
      (sf) =>
        !sf.isDeclarationFile &&
        fileSet.has(path.normalize(sf.fileName)) &&
        isUnderRoot(sf.fileName, repoRoot)
    );

  emitter.emitMetaData(basePath);
  const projectId = emitter.emitProject();

  // document vertex ID keyed by normalized file name
  const documentIds = new Map<string, number>();
  for (const sf of projectFiles) {
    const docId = emitter.emitDocument(sf.fileName);
    documentIds.set(path.normalize(sf.fileName), docId);
  }

  emitter.emitContains(projectId, Array.from(documentIds.values()));

  // symbol → LSIF result-set data (populated in pass 1)
  const symbolInfos = new Map<ts.Symbol, SymbolInfo>();

  // ── Pass 1: definitions ────────────────────────────────────────────────────
  for (const sf of projectFiles) {
    const defRangeIds: number[] = [];
    const ctx: DefCtx = {
      sf,
      docId: documentIds.get(path.normalize(sf.fileName))!,
      defRangeIds,
      symbolInfos,
      checker,
      emitter,
      basePath,
    };
    visitDef(sf, ctx);
    emitter.emitContains(ctx.docId, defRangeIds);
  }

  // ── Pass 2: references ────────────────────────────────────────────────────
  for (const sf of projectFiles) {
    const refRangeIds: number[] = [];
    const ctx: RefCtx = {
      sf,
      docId: documentIds.get(path.normalize(sf.fileName))!,
      refRangeIds,
      symbolInfos,
      checker,
      emitter,
    };
    visitRef(sf, ctx);
    emitter.emitContains(ctx.docId, refRangeIds);
  }
}

// ── Pass-1 visitor (module-level — one function object, no per-file allocation) ──

function visitDef(node: ts.Node, ctx: DefCtx): void {
  const [symbol, nameNode] = resolveDeclarationSymbol(node, ctx.checker);

  if (symbol && nameNode) {
    if (!ctx.symbolInfos.has(symbol)) {
      const vname = computeTravsrVName(node, ctx.sf, ctx.basePath);
      const resultSetId = ctx.emitter.emitResultSet(vname);
      const defResultId = ctx.emitter.emitDefinitionResult();
      const refResultId = ctx.emitter.emitReferenceResult();
      ctx.emitter.emitEdge('textDocument/definition', resultSetId, defResultId);
      ctx.emitter.emitEdge('textDocument/references', resultSetId, refResultId);
      ctx.symbolInfos.set(symbol, {
        resultSetId,
        definitionResultId: defResultId,
        referenceResultId: refResultId,
        documentId: ctx.docId,
      });
    }

    const info = ctx.symbolInfos.get(symbol)!;
    const rangeId = ctx.emitter.emitRange(ctx.sf, nameNode);
    ctx.emitter.emitEdge('next', rangeId, info.resultSetId);
    ctx.emitter.emitItem(info.definitionResultId, [rangeId], ctx.docId, 'definitions');
    ctx.defRangeIds.push(rangeId);
  }

  ts.forEachChild(node, (child) => visitDef(child, ctx));
}

// ── Pass-2 visitor (module-level — one function object, no per-file allocation) ──

function visitRef(node: ts.Node, ctx: RefCtx): void {
  // ── RefCall: call expressions ────────────────────────────────────────────
  if (ts.isCallExpression(node)) {
    const info = resolveRefTarget(node.expression, ctx.checker, ctx.symbolInfos);
    if (info) {
      const rangeId = ctx.emitter.emitRange(ctx.sf, node.expression);
      ctx.emitter.emitEdge('next', rangeId, info.resultSetId);
      ctx.emitter.emitItem(info.referenceResultId, [rangeId], ctx.docId, 'references');
      ctx.refRangeIds.push(rangeId);
    }
  }

  // ── RefImports: named import specifiers ──────────────────────────────────
  if (ts.isImportSpecifier(node)) {
    // Symbol resolution uses `node.propertyName ?? node.name` (the *imported*
    // name) to follow aliases: for `import { Foo as Bar }`, propertyName is
    // `Foo` and name is `Bar`. We resolve to Foo's resultSet so that every
    // usage of the alias appears as a reference to the original declaration.
    // The range is emitted on `node.name` (the local alias) — intentional:
    // the alias token is what the editor highlights on hover/go-to-refs.
    const importedName = node.propertyName ?? node.name;
    const info = resolveRefTarget(importedName, ctx.checker, ctx.symbolInfos);
    if (info) {
      const rangeId = ctx.emitter.emitRange(ctx.sf, node.name);
      ctx.emitter.emitEdge('next', rangeId, info.resultSetId);
      ctx.emitter.emitItem(info.referenceResultId, [rangeId], ctx.docId, 'references');
      ctx.refRangeIds.push(rangeId);
    }
  }

  // ── IsImplementation + Overrides ─────────────────────────────────────────
  if (ts.isClassDeclaration(node) && node.heritageClauses) {
    const classSymbol = node.name ? ctx.checker.getSymbolAtLocation(node.name) : undefined;

    for (const clause of node.heritageClauses) {
      // IsImplementation
      if (clause.token === ts.SyntaxKind.ImplementsKeyword) {
        for (const typeExpr of clause.types) {
          const ifaceType = ctx.checker.getTypeAtLocation(typeExpr);
          const ifaceSymbol = resolveAlias(ifaceType.getSymbol(), ctx.checker);
          if (!ifaceSymbol || !ctx.symbolInfos.has(ifaceSymbol)) continue;

          const ifaceInfo = ctx.symbolInfos.get(ifaceSymbol)!;

          // Lazily create implementationResult for this interface
          if (ifaceInfo.implementationResultId === undefined) {
            const implId = ctx.emitter.emitImplementationResult();
            ctx.emitter.emitEdge('textDocument/implementation', ifaceInfo.resultSetId, implId);
            ifaceInfo.implementationResultId = implId;
          }

          if (classSymbol && ctx.symbolInfos.has(classSymbol)) {
            const rangeId = ctx.emitter.emitRange(ctx.sf, typeExpr.expression);
            ctx.emitter.emitEdge('next', rangeId, ifaceInfo.resultSetId);
            ctx.emitter.emitItem(
              ifaceInfo.implementationResultId,
              [rangeId],
              ctx.docId,
              'implementationResults'
            );
            ctx.refRangeIds.push(rangeId);
          }
        }
      }

      // Overrides
      if (clause.token === ts.SyntaxKind.ExtendsKeyword) {
        for (const typeExpr of clause.types) {
          const baseType = ctx.checker.getTypeAtLocation(typeExpr);

          for (const member of node.members) {
            if (!ts.isMethodDeclaration(member) || !ts.isIdentifier(member.name)) continue;

            const baseMemberSymbol = ctx.checker.getPropertyOfType(baseType, member.name.text);
            const resolved = resolveAlias(baseMemberSymbol, ctx.checker);
            if (!resolved || !ctx.symbolInfos.has(resolved)) continue;

            const baseInfo = ctx.symbolInfos.get(resolved)!;
            const rangeId = ctx.emitter.emitRange(ctx.sf, member.name);
            ctx.emitter.emitEdge('next', rangeId, baseInfo.resultSetId);
            ctx.emitter.emitItem(baseInfo.referenceResultId, [rangeId], ctx.docId, 'references');
            ctx.refRangeIds.push(rangeId);
          }
        }
      }
    }
  }

  ts.forEachChild(node, (child) => visitRef(child, ctx));
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/**
 * Compute the Travsr VName for a declaration node.
 *
 * Signature format is identical to travsr-indexer/src/emit.rs so that the Rust
 * LSIF ingester can deterministically compute the same BLAKE3 NodeId and
 * match LSIF resultSets to Tree-sitter-indexed nodes without a store lookup.
 *
 *   class:ClassName          → ClassDeclaration
 *   fn:funcName              → FunctionDeclaration
 *   method:ClassName.name    → MethodDeclaration (requires parent class walk)
 *   var:varName              → VariableDeclaration
 *   interface:InterfaceName  → InterfaceDeclaration (not indexed by Tree-sitter; included for future use)
 *
 * Returns undefined for unrecognised node kinds — the caller omits travsr_vname
 * in that case, so the resultSet is still valid LSIF, just opaque to the Rust side.
 */
function computeTravsrVName(
  node: ts.Node,
  sf: ts.SourceFile,
  basePath: string
): { path: string; signature: string } | undefined {
  const relPath = path.relative(basePath, sf.fileName).replace(/\\/g, '/');
  let signature: string;

  if (ts.isClassDeclaration(node) && node.name) {
    signature = `class:${node.name.text}`;
  } else if (ts.isInterfaceDeclaration(node) && node.name) {
    signature = `interface:${node.name.text}`;
  } else if (ts.isFunctionDeclaration(node) && node.name) {
    signature = `fn:${node.name.text}`;
  } else if (ts.isMethodDeclaration(node) && ts.isIdentifier(node.name)) {
    const className = findParentClassName(node) ?? '<anonymous>';
    signature = `method:${className}.${node.name.text}`;
  } else if (ts.isVariableDeclaration(node) && ts.isIdentifier(node.name)) {
    signature = `var:${node.name.getText(sf)}`;
  } else {
    return undefined;
  }

  return { path: relPath, signature };
}

/** Walk up the AST from a method node to find the enclosing class name. */
function findParentClassName(node: ts.Node): string | undefined {
  let current: ts.Node | undefined = node.parent;
  while (current) {
    if (ts.isClassDeclaration(current) && current.name) {
      return current.name.text;
    }
    current = current.parent;
  }
  return undefined;
}

/**
 * Given a declaration node, return [symbol, nameNode] if it is a named
 * declaration kind we want to index, otherwise [undefined, undefined].
 */
function resolveDeclarationSymbol(
  node: ts.Node,
  checker: ts.TypeChecker
): [ts.Symbol | undefined, ts.Node | undefined] {
  if (ts.isClassDeclaration(node) && node.name) {
    return [checker.getSymbolAtLocation(node.name), node.name];
  }
  if (ts.isInterfaceDeclaration(node) && node.name) {
    return [checker.getSymbolAtLocation(node.name), node.name];
  }
  if (ts.isFunctionDeclaration(node) && node.name) {
    return [checker.getSymbolAtLocation(node.name), node.name];
  }
  if (ts.isMethodDeclaration(node) && ts.isIdentifier(node.name)) {
    return [checker.getSymbolAtLocation(node.name), node.name];
  }
  if (ts.isVariableDeclaration(node) && ts.isIdentifier(node.name)) {
    return [checker.getSymbolAtLocation(node.name), node.name];
  }
  return [undefined, undefined];
}

/**
 * Resolve a reference node to a SymbolInfo entry from pass 1.
 * Follows alias chains so `import { Foo }` resolves to Foo's declaration.
 */
function resolveRefTarget(
  node: ts.Node,
  checker: ts.TypeChecker,
  symbolInfos: Map<ts.Symbol, SymbolInfo>
): SymbolInfo | undefined {
  const raw = checker.getSymbolAtLocation(node);
  const resolved = resolveAlias(raw, checker);
  return resolved ? symbolInfos.get(resolved) : undefined;
}

/** Follow alias chain; returns undefined if input is undefined. */
function resolveAlias(
  sym: ts.Symbol | undefined,
  checker: ts.TypeChecker
): ts.Symbol | undefined {
  if (!sym) return undefined;
  if ((sym.flags & ts.SymbolFlags.Alias) !== 0) {
    return checker.getAliasedSymbol(sym);
  }
  return sym;
}
