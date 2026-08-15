import { makeGreeter, Greeter } from "./greeter";

export function run(): string {
  const g: Greeter = makeGreeter("world");
  return g.greet().text;
}
