import { makeGreeter, Greeter, Registry, firstOf, formatGreeting } from "./index";
import type { Greeting } from "./index";

export async function run(): Promise<string> {
  const g: Greeter = makeGreeter("world");
  const greeting: Greeting = g.speak();
  const later = await g.speakLater();
  const described = g.describe();
  const lbl = g.label;

  const reg = new Registry<string>();
  reg.add(greeting.text);
  const head = firstOf(reg.all());

  return [formatGreeting(greeting), later.text, described, lbl, head].join("|");
}
