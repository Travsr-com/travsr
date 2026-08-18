/**
 * Streaming LSIF emitter — writes one JSON Line per vertex/edge to a
 * WritableStream (defaults to process.stdout).
 *
 * LSIF (Language Server Index Format) encodes a code-intelligence graph as a
 * sequence of JSON Lines: every line is either a "vertex" (node) or an "edge"
 * (directed link). IDs are monotonically increasing integers scoped to a
 * single dump. We emit in the order the walker produces records so the file
 * stays streamable: consumers never need to buffer the full dump.
 *
 * Spec ref: https://microsoft.github.io/language-server-protocol/specifications/lsif/0.4.0/specification/
 */

import * as ts from 'typescript';

/**
 * Strip a Windows extended-length path prefix (`\\?\`, `\\?\UNC\`) so an emitted
 * `file://` URI carries a plain absolute path. TypeScript's realpath yields these
 * verbatim prefixes on Windows; left in the document URI they reach the Rust
 * ingest as `file://\\?\C:\...`, which never relativizes against the plain project
 * root, so occurrence paths surfaced as `//?/C:/...` instead of repo-relative. The
 * Python emitter never hits this (its files come from a plain directory walk), so
 * only TypeScript was affected. No-op on macOS/Linux, where the prefix never appears.
 */
export function stripExtendedLengthPrefix(p: string): string {
  // TypeScript normalizes paths to forward slashes internally, so a verbatim
  // project path reaches us as `//?/C:/...` (and `//?/UNC/...`), not the raw
  // `\\?\C:\...`. Strip BOTH shapes so the emitted `file://` URI is a plain
  // absolute path the Rust ingest can relativize. No-op on macOS/Linux.
  for (const unc of ['\\\\?\\UNC\\', '//?/UNC/']) {
    if (p.startsWith(unc)) return '//' + p.slice(unc.length);
  }
  for (const dev of ['\\\\?\\', '//?/']) {
    if (p.startsWith(dev)) return p.slice(dev.length);
  }
  return p;
}

export class Emitter {
  private idCounter = 0;
  private readonly write: (line: string) => void;

  constructor(out: NodeJS.WritableStream = process.stdout) {
    this.write = (line) => out.write(line + '\n');
  }

  private nextId(): number {
    return ++this.idCounter;
  }

  private emit(obj: Record<string, unknown>): number {
    this.write(JSON.stringify(obj));
    return obj['id'] as number;
  }

  emitMetaData(projectRoot: string): number {
    const id = this.nextId();
    return this.emit({
      id,
      type: 'vertex',
      label: 'metaData',
      version: '0.4.3',
      projectRoot: `file://${stripExtendedLengthPrefix(projectRoot)}`,
      positionEncoding: 'utf-16',
      toolInfo: { name: 'travsr-lsif-ts', version: '0.1.0' },
    });
  }

  emitProject(): number {
    const id = this.nextId();
    return this.emit({ id, type: 'vertex', label: 'project', kind: 'typescript' });
  }

  emitDocument(fileName: string): number {
    const id = this.nextId();
    return this.emit({
      id,
      type: 'vertex',
      label: 'document',
      uri: `file://${stripExtendedLengthPrefix(fileName)}`,
      languageId: 'typescript',
    });
  }

  /**
   * Emit a resultSet vertex, optionally annotating it with a Travsr VName.
   *
   * The `travsr_vname` field is a non-standard extension: LSIF consumers that
   * don't know Travsr ignore it (the field is not in the core spec). The Rust
   * ingester in travsr-indexer uses it to map LSIF symbols to Travsr NodeIds
   * without requiring a separate position-lookup pass.
   *
   * Signature format mirrors the Tree-sitter indexer in travsr-indexer/src/emit.rs:
   *   class:ClassName  |  fn:funcName  |  method:ClassName.methodName  |  var:varName
   */
  emitResultSet(vname?: { path: string; signature: string }): number {
    const id = this.nextId();
    const vertex: Record<string, unknown> = { id, type: 'vertex', label: 'resultSet' };
    if (vname) {
      vertex['travsr_vname'] = vname;
    }
    return this.emit(vertex);
  }

  /**
   * Emit a range vertex from a TypeScript AST node.
   * Uses getStart(sourceFile) to skip leading trivia so the range covers
   * only the token text, matching what editors highlight on hover.
   */
  emitRange(sourceFile: ts.SourceFile, node: ts.Node): number {
    const id = this.nextId();
    const startPos = node.getStart(sourceFile);
    const endPos = node.getEnd();
    const start = sourceFile.getLineAndCharacterOfPosition(startPos);
    const end = sourceFile.getLineAndCharacterOfPosition(endPos);
    return this.emit({
      id,
      type: 'vertex',
      label: 'range',
      start: { line: start.line, character: start.character },
      end: { line: end.line, character: end.character },
    });
  }

  emitDefinitionResult(): number {
    const id = this.nextId();
    return this.emit({ id, type: 'vertex', label: 'definitionResult' });
  }

  emitReferenceResult(): number {
    const id = this.nextId();
    return this.emit({ id, type: 'vertex', label: 'referenceResult' });
  }

  emitImplementationResult(): number {
    const id = this.nextId();
    return this.emit({ id, type: 'vertex', label: 'implementationResult' });
  }

  /** Single-target edge (next, textDocument/definition, textDocument/references, …). */
  emitEdge(label: string, outV: number, inV: number): number {
    const id = this.nextId();
    return this.emit({ id, type: 'edge', label, outV, inV });
  }

  /** Multi-target contains edge (project→documents, document→ranges). */
  emitContains(outV: number, inVs: number[]): number | undefined {
    if (inVs.length === 0) return undefined;
    const id = this.nextId();
    return this.emit({ id, type: 'edge', label: 'contains', outV, inVs });
  }

  /**
   * Item edge linking a result vertex to the ranges it contains.
   * property is "definitions", "references", or "implementationResults".
   */
  emitItem(outV: number, inVs: number[], document: number, property: string): number | undefined {
    if (inVs.length === 0) return undefined;
    const id = this.nextId();
    return this.emit({ id, type: 'edge', label: 'item', outV, inVs, document, property });
  }
}
