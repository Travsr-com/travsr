import { IRepository } from './interfaces';
import { User } from './types';

export class UserRepository implements IRepository<User> {
  private store = new Map<string, User>();

  // A callable field (arrow-function property), invoked directly below as
  // `this.onSave()`. It is both a call and a property access, so it exercises
  // the RefCall / RefMember overlap guard in the emitter.
  private onSave = (): void => {};

  async findById(id: string): Promise<User | null> {
    return this.store.get(id) ?? null;
  }

  async save(entity: User): Promise<void> {
    this.store.set(entity.id, entity);
    this.onSave();
  }

  async delete(id: string): Promise<void> {
    this.store.delete(id);
  }
}
