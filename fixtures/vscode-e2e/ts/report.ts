// FROZEN FIXTURE - do not reformat. expected.json pins line numbers.
// Second definition of `format` - same name, different file. See util.ts.
// This one has zero callers; util.ts's has one. A tool that merges the two
// will report one caller here and the case fails.
export function format(rows: string[]): string {
  return rows.join(",");
}
