import scaleValue, { addNumbers, Accumulator, addLater, VERSION } from "./barrel.mjs";

export async function run() {
  const total = addNumbers(2, 3);
  const acc = Accumulator.zero().add(total);
  const later = await addLater(1, 2);
  return `${VERSION}:${scaleValue(total, 4)}:${acc.total}:${later}`;
}

export async function lazy() {
  // Dynamic import: resolved at runtime, not by a static import statement.
  const mod = await import("./util.mjs");
  return mod.addNumbers(1, 1);
}
