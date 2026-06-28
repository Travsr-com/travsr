/**
 * Ambient declaration for tree-sitter-python — the npm package ships no
 * TypeScript types.  It re-exports a Parser.Language grammar object that can
 * be passed directly to Parser.setLanguage().
 */
declare module 'tree-sitter-python' {
  import type Parser from 'tree-sitter';
  const language: Parser.Language;
  export = language;
}
