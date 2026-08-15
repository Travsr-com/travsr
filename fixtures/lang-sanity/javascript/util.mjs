// ESM: named, default, class, async, and a re-exported binding.
export function addNumbers(a, b) {
  return a + b;
}

export const VERSION = "1.0.0";

export class Accumulator {
  #total = 0;

  add(n) {
    this.#total = addNumbers(this.#total, n);
    return this;
  }

  get total() {
    return this.#total;
  }

  static zero() {
    return new Accumulator();
  }
}

export async function addLater(a, b) {
  return addNumbers(a, b);
}

export default function scaleValue(v, factor) {
  return addNumbers(v, v) * factor;
}
