import { BaseService } from './base-service';
import { ILogger, IRepository } from './interfaces';
import { User, Result } from './types';

export class UserService extends BaseService {
  private repo: IRepository<User>;

  constructor(logger: ILogger, repo: IRepository<User>) {
    super(logger);
    this.repo = repo;
  }

  getName(): string {
    return 'UserService';
  }

  async getUser(id: string): Promise<Result<User>> {
    try {
      const user = await this.repo.findById(id);
      if (!user) {
        return { ok: false, error: new Error(`User ${id} not found`) };
      }
      this.logger.log(`Retrieved user ${id}`);
      return { ok: true, value: user };
    } catch (err) {
      this.logger.error('Failed to retrieve user', err as Error);
      return { ok: false, error: err as Error };
    }
  }

  async createUser(user: User): Promise<void> {
    await this.repo.save(user);
    this.logger.log(`Created user ${user.id}`);
  }
}
