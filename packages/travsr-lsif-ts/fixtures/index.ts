import { ConsoleLogger } from './console-logger';
import { UserRepository } from './user-repository';
import { UserService } from './user-service';
import { AuthService } from './auth-service';
import { AuthMiddleware } from './middleware';
import { UserController } from './controller';

function bootstrap(): UserController {
  const logger = new ConsoleLogger();
  const repo = new UserRepository();
  const userService = new UserService(logger, repo);
  const authService = new AuthService(logger);
  const middleware = new AuthMiddleware(authService, logger);

  userService.initialize();
  authService.initialize();

  return new UserController(userService, authService, middleware);
}

export { bootstrap };
