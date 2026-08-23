/**
 * Fixture for the travsr_vname variable-classification rules (issue #755
 * item 1). Every shape here pins one branch of computeTravsrVName's
 * variable handling against tree-sitter's `topvar` rules:
 *
 *   - top-level arrow const           → fn:shout       (NOT var:shout)
 *   - top-level function-expression   → fn:legacyShout (NOT var:legacyShout)
 *   - top-level plain const           → var:MAX_VOLUME
 *   - local variable inside a body    → no travsr_vname at all
 *
 * The orphan-edge bug this guards: the emitter said `var:shout` while
 * tree-sitter indexed `fn:shout`, so every reference to an arrow-function
 * const became a ref/call edge to a node id that was never written.
 */

export const shout = (s: string): string => s.toUpperCase();

export const legacyShout = function (s: string): string {
  return s.toUpperCase();
};

export const MAX_VOLUME = 11;

export function useHelpers(input: string): string {
  // A local: tree-sitter drops non-top-level declarators entirely, so this
  // must get no travsr_vname (a vname would orphan any reference to it).
  const localEcho = (s: string): string => s + s;
  return localEcho(shout(input)) + legacyShout(input) + String(MAX_VOLUME);
}
