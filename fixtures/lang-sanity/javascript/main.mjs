// ESM import across files: the edge Phase B has to resolve.
import scaleValue, { addNumbers, VERSION } from "./util.mjs";

export function run() {
  const total = addNumbers(2, 3);
  return `${VERSION}:${scaleValue(total, 4)}`;
}
