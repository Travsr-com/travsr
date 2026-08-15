export interface Greeting {
  text: string;
}

export class Greeter {
  constructor(private readonly name: string) {}

  greet(): Greeting {
    return { text: `hello ${this.name}` };
  }
}

export function makeGreeter(name: string): Greeter {
  return new Greeter(name);
}
