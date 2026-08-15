// Barrel file: re-export, so imports resolve one hop away from the definition.
export { makeGreeter, Greeter, Registry, firstOf, formatGreeting } from "./greeter";
export type { Greeting, Formatter } from "./greeter";
