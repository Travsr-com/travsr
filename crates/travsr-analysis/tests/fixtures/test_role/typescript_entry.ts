// #479 golden fixture — TypeScript, parsed under a `*.test.ts` vname path.
//
// v1 has no EntryPoint for TS (BDD `it()`/`test()` callbacks emit no node,
// §9). The whole test file is a Support scope instead.

export class CalibrationSuite {
  run(): void {}
}

// Support: a setup helper in a test file.
export function setupFixture(): void {}
