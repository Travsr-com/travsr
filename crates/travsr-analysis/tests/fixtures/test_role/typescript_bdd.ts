// #674 golden fixture - TypeScript BDD callbacks (Jest / Vitest / Mocha).
//
// Parsed twice: once under a `*.test.ts` path and once under a production path.
// The `describe`/`it`/`test` AST rule alone has to classify these, so the
// production-path run is the one that proves no path gate is involved.

import { charge } from "./billing";

describe("Payments", () => {
  // Support: a setup helper inside the suite scope, not an entry point.
  function buildCart(): number {
    return 1;
  }

  it("charges the card", () => {
    charge(buildCart());
  });

  describe("refunds", () => {
    test("refunds the card", function () {
      charge(buildCart());
    });
  });

  // No node: a template literal name is not stable across re-runs, so the
  // callback falls back to the suite's Support classification.
  it(`renders ${1} row`, () => {});
});

// None: production code in the same file, outside every suite.
export function calibrateFloors(): void {}
