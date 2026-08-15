export interface Greeting {
  text: string;
}

export type Formatter = (g: Greeting) => string;

export enum Tone {
  Warm = "warm",
  Terse = "terse",
}

export abstract class Speaker {
  abstract speak(): Greeting;

  describe(): string {
    return this.speak().text;
  }
}

export class Greeter extends Speaker implements Speaker {
  static created = 0;

  constructor(private readonly name: string, readonly tone: Tone = Tone.Warm) {
    super();
    Greeter.created += 1;
  }

  speak(): Greeting {
    return { text: `hello ${this.name}` };
  }

  get label(): string {
    return this.name;
  }

  async speakLater(): Promise<Greeting> {
    return this.speak();
  }
}

/** Generic function. */
export function firstOf<T>(items: T[]): T | undefined {
  return items[0];
}

/** Generic class. */
export class Registry<T> {
  private items: T[] = [];
  add(item: T): void {
    this.items.push(item);
  }
  all(): T[] {
    return this.items;
  }
}

export function makeGreeter(name: string): Greeter {
  return new Greeter(name);
}

export const formatGreeting: Formatter = (g) => g.text.toUpperCase();
