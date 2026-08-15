// CommonJS: require/module.exports, the flavour `.cjs` forces.
const { sumLegacy } = require("./util.cjs");

function legacyTotal(a, b) {
  return sumLegacy(a, b);
}

module.exports = { legacyTotal };
