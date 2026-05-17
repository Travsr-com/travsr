/**
 * TypeScript compiler API walker — two-pass LSIF emitter.
 *
 * Pass 1  (definitions): walks every project source file and emits a
 *   resultSet + definitionResult + referenceResult for each named declaration
 *   (class, interface, function, method, variable). Ranges at definition sites
 *   are linked via `next` and `item/definitions`.
 *
 * Pass 2  (references): re-walks every file and emits:
 *   - RefCall      — call expressions resolved to a project declaration
 *   - RefImports   — named import specifiers resolved to a project declaration
 *   - IsImplementation — `implements` clauses resolved to a project interface
 *   - Overrides    — method declarations that shadow a base-class method
 *
 * The two-pass design is necessary because a reference in file A can point to
 * a declaration in file B that hasn't been visited yet in a single pass.
 */

import * as ts from 'typescript';
import * as path from 'path';
import { Emitter } from './emitter';

interface SymbolInfo {
  resultSetId: number;
  definitionResultId: number;
  referenceResultId: number;
  /** Set lazily when the first `implements` clause targets this symbol. */
  implementationResultId?: number;
  /** Document vertex ID of the file that contains the primary declaration. */
  documentId: number;
}

export function walk(tsconfigPath: string, emitter: Emitter): void {
  const configFile = ts.readConfigFile(tsconfigPath, ts.sys.readFile);
  if (configFile.error) {
    throw new Error(
      `tsconfig parse error: ${ts.flattenDiagnosticMessageText(configFile.error.messageText, '\n')}`
    );
  }

  const basePath = path.dirname(tsconfigPath);
  const parsed = ts.parseJsonConfigFileContent(configFile.config, ts.sys, basePath);
  const fileSet = new Set(parsed.fileNames.map((f) => path.normalize(f)));

  const program = ts.createProgram(parsed.fileNames, parsed.options);
  const checker = program.getTypeChecker();

  const projectFiles = program
    .getSourceFiles()
    .filter((sf) => !sf.isDeclarationFile && fileSet.has(path.normalize(sf.fileName)));

  emitter.emitMetaData(basePath);
  const projectId = emitter.emitProject();

  // document vertex ID keyed by normalized file name
  const documentIds = new Map<string, number>();
  for (const sf of projectFiles) {
    const docId = emitter.emitDocument(sf.fileName);
    documentIds.set(path.normalize(sf.fileName), docId);
  }

  emitter.emitContains(
    projectId,
    Array.from(documentIds.values())
  );

  // symbol → LSIF result-set data (populated in pass 1)
  const symbolInfos = new Map<ts.Symbol, SymbolInfo>();

  // ── Pass 1: definitions ────────────────────────────────────────────────────
  for (const sf of projectFiles) {
    const docId = documentIds.get(path.normalize(sf.fileName))!;
    const defRangeIds: number[] = [];

    function visitDef(node: ts.Node): void {
      const [symbol, nameNode] = resolveDeclarationSymbol(node, checker);

      if (symbol && nameNode) {
        if (!symbolInfos.has(symbol)) {
          const resultSetId = emitter.emitResultSet();
          const defResultId = emitter.emitDefinitionResult();
          const refResultId = emitter.emitReferenceResult();
          emitter.emitEdge('textDocument/definition', resultSetId, defResultId);
          emitter.emitEdge('textDocument/references', resultSetId, refResultId);
          symbolInfos.set(symbol, {
            resultSetId,
            definitionResultId: defResultId,
            referenceResultId: refResultId,
            documentId: docId,
          });
        }

        const info = symbolInfos.get(symbol)!;
        const rangeId = emitter.emitRange(sf, nameNode);
        emitter.emitEdge('next', rangeId, info.resultSetId);
        emitter.emitItem(info.definitionResultId, [rangeId], docId, 'definitions');
        defRangeIds.push(rangeId);
      }

      ts.forEachChild(node, visitDef);
    }

    visitDef(sf);

    if (defRangeIds.length > 0) {
      emitter.emitContains(docId, defRangeIds);
    }
  }

  // ── Pass 2: references ────────────────────────────────────────────────────
  for (const sf of projectFiles) {
    const docId = documentIds.get(path.normalize(sf.fileName))!;
    const refRangeIds: number[] = [];

    function visitRef(node: ts.Node): void {
      // ── RefCall: call expressions ──────────────────────────────────────────
      if (ts.isCallExpression(node)) {
        const info = resolveRefTarget(node.expression, checker, symbolInfos);
        if (info) {
          const rangeId = emitter.emitRange(sf, node.expression);
          emitter.emitEdge('next', rangeId, info.resultSetId);
          emitter.emitItem(info.referenceResultId, [rangeId], docId, 'references');
          refRangeIds.push(rangeId);
        }
      }

      // ── RefImports: named import specifiers ────────────────────────────────
      if (ts.isImportSpecifier(node)) {
        // `node.name` is the local alias; the imported name is `node.propertyName ?? node.name`
        const importedName = node.propertyName ?? node.name;
        const info = resolveRefTarget(importedName, checker, symbolInfos);
        if (info) {
          const rangeId = emitter.emitRange(sf, node.name);
          emitter.emitEdge('next', rangeId, info.resultSetId);
          emitter.emitItem(info.referenceResultId, [rangeId], docId, 'references');
          refRangeIds.push(rangeId);
        }
      }

      // ── IsImplementation + Overrides ───────────────────────────────────────
      if (ts.isClassDeclaration(node) && node.heritageClauses) {
        const classSymbol = node.name ? checker.getSymbolAtLocation(node.name) : undefined;

        for (const clause of node.heritageClauses) {
          // IsImplementation
          if (clause.token === ts.SyntaxKind.ImplementsKeyword) {
            for (const typeExpr of clause.types) {
              const ifaceType = checker.getTypeAtLocation(typeExpr);
              const ifaceSymbol = resolveAlias(ifaceType.getSymbol(), checker);
              if (!ifaceSymbol || !symbolInfos.has(ifaceSymbol)) continue;

              const ifaceInfo = symbolInfos.get(ifaceSymbol)!;

              // Lazily create implementationResult for this interface
              if (ifaceInfo.implementationResultId === undefined) {
                const implId = emitter.emitImplementationResult();
                emitter.emitEdge('textDocument/implementation', ifaceInfo.resultSetId, implId);
                ifaceInfo.implementationResultId = implId;
              }

              if (classSymbol && symbolInfos.has(classSymbol)) {
                const rangeId = emitter.emitRange(sf, typeExpr.expression);
                emitter.emitEdge('next', rangeId, ifaceInfo.resultSetId);
                emitter.emitItem(
                  ifaceInfo.implementationResultId,
                  [rangeId],
                  docId,
                  'implementationResults'
                );
                refRangeIds.push(rangeId);
              }
            }
          }

          // Overrides
          if (clause.token === ts.SyntaxKind.ExtendsKeyword) {
            for (const typeExpr of clause.types) {
              const baseType = checker.getTypeAtLocation(typeExpr);

              for (const member of node.members) {
                if (!ts.isMethodDeclaration(member) || !ts.isIdentifier(member.name)) continue;

                const baseMemberSymbol = checker.getPropertyOfType(
                  baseType,
                  member.name.text
                );
                const resolved = resolveAlias(baseMemberSymbol, checker);
                if (!resolved || !symbolInfos.has(resolved)) continue;

                const baseInfo = symbolInfos.get(resolved)!;
                const rangeId = emitter.emitRange(sf, member.name);
                emitter.emitEdge('next', rangeId, baseInfo.resultSetId);
                emitter.emitItem(baseInfo.referenceResultId, [rangeId], docId, 'references');
                refRangeIds.push(rangeId);
              }
            }
          }
        }
      }

      ts.forEachChild(node, visitRef);
    }

    visitRef(sf);

    if (refRangeIds.length > 0) {
      emitter.emitContains(docId, refRangeIds);
    }
  }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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
