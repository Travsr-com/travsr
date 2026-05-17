import { BaseService } from './base-service';
import { ILogger } from './interfaces';
import { AuthToken, User } from './types';

export class AuthService extends BaseService {
  private tokens = new Map<string, AuthToken>();

  constructor(logger: ILogger) {
    super(logger);
  }

  getName(): string {
    return 'AuthService';
  }

  /** Overrides BaseService.initialize to add a readiness log after super call. */
  initialize(): void {
    super.initialize();
    this.logger.log('AuthService: token store ready');
  }

  generateToken(user: User): AuthToken {
    const token: AuthToken = {
      token: `tok_${user.id}_${Date.now()}`,
      expiresAt: new Date(Date.now() + 3_600_000),
      userId: user.id,
    };
    this.tokens.set(token.token, token);
    this.logger.log(`Generated token for ${user.id}`);
    return token;
  }

  validateToken(tokenStr: string): AuthToken | null {
    const token = this.tokens.get(tokenStr);
    if (!token) return null;
    if (token.expiresAt < new Date()) {
      this.tokens.delete(tokenStr);
      return null;
    }
    return token;
  }
}
