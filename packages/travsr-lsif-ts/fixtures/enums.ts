export enum Level {
  Low,
  High = 2,
}

export function isHigh(l: Level): boolean {
  return l === Level.High;
}
