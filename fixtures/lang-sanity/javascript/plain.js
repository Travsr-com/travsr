// Plain .js: no module syntax at all, the ambient script flavour.
function plainHelper(value) {
  return value * 2;
}

function plainCaller(value) {
  return plainHelper(value) + 1;
}
