// CommonJS: require/module.exports, the flavour `.cjs` forces.
const { sumLegacy, LegacyBag } = require("./util.cjs");

function legacyTotal(a, b) {
  return sumLegacy(a, b);
}

function legacyBag(v) {
  return new LegacyBag().push(v);
}

module.exports = { legacyTotal, legacyBag };
