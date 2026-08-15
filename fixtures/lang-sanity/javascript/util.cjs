// Distinct names from the ESM side on purpose: a name defined in two files is
// deliberately left unindexed to avoid mis-targeting, which would confound a
// CommonJS-vs-ESM comparison with an ambiguity result.
function sumLegacy(a, b) {
  return a + b;
}

class LegacyBag {
  constructor() {
    this.items = [];
  }
  push(v) {
    this.items.push(v);
    return this;
  }
}

module.exports = { sumLegacy, LegacyBag };
