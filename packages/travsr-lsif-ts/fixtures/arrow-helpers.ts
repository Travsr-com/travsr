/**
 * Fixture for the travsr_vname variable-classification rules (issue #755
 * item 1). Every shape here pins one branch of computeTravsrVName's
 * variable handling against tree-sitter's `topvar` rules:
 *
 *   - top-level arrow const           → fn:shout        (NOT var:shout)
 *   - top-level function-expression   → fn:legacyShout  (NOT var:legacyShout)
 *   - top-level generator expression  → var:genShout    (NOT fn:genShout)
 *   - top-level plain const           → var:MAX_VOLUME
 *   - ambient `declare const`         → no travsr_vname at all
 *   - local variable inside a body    → no travsr_vname at all
 *
 * The orphan-edge bug this guards: the emitter said `var:shout` while
 * tree-sitter indexed `fn:shout`, so every reference to an arrow-function
 * const became a ref/call edge to a node id that was never written.
 *
 * The generator and ambient rows are the mirror of that, and both bite in the
 * same direction. `function*` is a `FunctionExpression` in the TS AST but a
 * distinct `generator_function` kind in the tree-sitter grammar, so calling it
 * `fn:` names a node written as `var:`. `declare const` is wrapped in
 * `ambient_declaration`, so no `@topvar` pattern matches and tree-sitter writes
 * no node at all, making any vname for it an orphan.
 */

export const shout = (s: string): string => s.toUpperCase();

export const legacyShout = function (s: string): string {
  return s.toUpperCase();
};

export const genShout = function* (s: string): Generator<string> {
  yield s.toUpperCase();
};

export const MAX_VOLUME = 11;

declare const AMBIENT_LIMIT: number;

export function useHelpers(input: string): string {
  // A local: tree-sitter drops non-top-level declarators entirely, so this
  // must get no travsr_vname (a vname would orphan any reference to it).
  const localEcho = (s: string): string => s + s;
  return (
    localEcho(shout(input)) +
    legacyShout(input) +
    String(MAX_VOLUME) +
    String(AMBIENT_LIMIT) +
    String(genShout(input).next().value)
  );
}
