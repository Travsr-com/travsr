import { UserService } from './user-service';
import { AuthService } from './auth-service';
import { AuthMiddleware } from './middleware';
import { User } from './types';

export class UserController {
  private userService: UserService;
  private authService: AuthService;
  private middleware: AuthMiddleware;

  constructor(
    userService: UserService,
    authService: AuthService,
    middleware: AuthMiddleware
  ) {
    this.userService = userService;
    this.authService = authService;
    this.middleware = middleware;
  }

  async getUser(token: string, userId: string): Promise<User | null> {
    const auth = this.middleware.check(token);
    if (!auth) return null;

    const result = await this.userService.getUser(userId);
    return result.ok ? result.value : null;
  }

  async createAndAuth(user: User): Promise<string | null> {
    await this.userService.createUser(user);
    const tok = this.authService.generateToken(user);
    return tok.token;
  }
}
