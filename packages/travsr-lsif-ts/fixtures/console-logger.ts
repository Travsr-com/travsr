import { ILogger } from './interfaces';

export class ConsoleLogger implements ILogger {
  log(message: string): void {
    console.log(`[LOG] ${message}`);
  }

  error(message: string, err?: Error): void {
    console.error(`[ERR] ${message}`, err?.message ?? '');
  }
}
