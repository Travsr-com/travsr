export interface ILogger {
  log(message: string): void;
  error(message: string, err?: Error): void;
}

export interface IRepository<T> {
  findById(id: string): Promise<T | null>;
  save(entity: T): Promise<void>;
  delete(id: string): Promise<void>;
}
