import { AuthService } from './auth-service';
import { ILogger } from './interfaces';
import { AuthToken } from './types';

export class AuthMiddleware {
  private auth: AuthService;
  private logger: ILogger;

  constructor(auth: AuthService, logger: ILogger) {
    this.auth = auth;
    this.logger = logger;
  }

  check(tokenStr: string): AuthToken | null {
    const result = this.auth.validateToken(tokenStr);
    if (!result) {
      this.logger.error('Invalid or expired token');
    }
    return result;
  }
}
