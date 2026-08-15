// ESM: named + default exports, the flavour `.mjs` forces.
export function addNumbers(a, b) {
  return a + b;
}

export const VERSION = "1.0.0";

export default function scaleValue(v, factor) {
  return addNumbers(v, v) * factor;
}
