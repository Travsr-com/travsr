/**
 * Streaming LSIF emitter for Python — writes one JSON Line per vertex/edge.
 *
 * Structurally identical to travsr-lsif-ts/emitter.ts; the only difference is
 * that emitRange() takes a tree-sitter SyntaxNode (with .startPosition /
 * .endPosition rows and columns) instead of a TypeScript compiler ts.Node.
 *
 * The `travsr_vname` extension on resultSet vertices is the same format used
 * by the Rust ingester (travsr-indexer/src/lsif.rs) so that NodeIds produced
 * here match the NodeIds from tree-sitter Phase A parsing — no store lookup
 * required during LSIF ingestion.
 *
 * Signature format (mirrors travsr-analysis/src/python.rs emit.rs):
 *   fn:funcName              — module-level function
 *   class:ClassName          — class definition
 *   method:ClassName.name    — method inside a class
 */

import type Parser from 'tree-sitter';

type SyntaxNode = Parser.SyntaxNode;

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
      projectRoot: `file://${projectRoot}`,
      positionEncoding: 'utf-16',
      toolInfo: { name: 'travsr-lsif-py', version: '0.1.0' },
    });
  }

  emitProject(): number {
    const id = this.nextId();
    return this.emit({ id, type: 'vertex', label: 'project', kind: 'python' });
  }

  emitDocument(fileName: string): number {
    const id = this.nextId();
    return this.emit({
      id,
      type: 'vertex',
      label: 'document',
      uri: `file://${fileName}`,
      languageId: 'python',
    });
  }

  /**
   * Emit a resultSet vertex, optionally tagging it with a Travsr VName.
   *
   * The `travsr_vname` field is a non-standard extension that the Rust LSIF
   * ingester uses to match LSIF resultSets to tree-sitter Phase A NodeIds
   * without querying the store.  LSIF consumers that don't know Travsr
   * silently ignore the field.
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
   * Emit a range vertex from a tree-sitter SyntaxNode.
   * startPosition / endPosition are already 0-indexed row+column — no
   * conversion required (unlike the TypeScript emitter which calls
   * getLineAndCharacterOfPosition on the compiler's SourceFile).
   */
  emitRange(node: SyntaxNode): number {
    const id = this.nextId();
    return this.emit({
      id,
      type: 'vertex',
      label: 'range',
      start: { line: node.startPosition.row, character: node.startPosition.column },
      end: { line: node.endPosition.row, character: node.endPosition.column },
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

  /** Item edge linking a result vertex to the ranges it contains. */
  emitItem(outV: number, inVs: number[], document: number, property: string): number | undefined {
    if (inVs.length === 0) return undefined;
    const id = this.nextId();
    return this.emit({ id, type: 'edge', label: 'item', outV, inVs, document, property });
  }
}
