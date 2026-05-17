import { ILogger } from './interfaces';

export abstract class BaseService {
  protected logger: ILogger;

  constructor(logger: ILogger) {
    this.logger = logger;
  }

  abstract getName(): string;

  initialize(): void {
    this.logger.log(`[${this.getName()}] initializing`);
  }

  shutdown(): void {
    this.logger.log(`[${this.getName()}] shutting down`);
  }
}
