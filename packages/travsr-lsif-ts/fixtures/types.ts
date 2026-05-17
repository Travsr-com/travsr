export interface User {
  id: string;
  name: string;
  email: string;
  createdAt: Date;
}

export interface AuthToken {
  token: string;
  expiresAt: Date;
  userId: string;
}

export type Result<T> = { ok: true; value: T } | { ok: false; error: Error };
