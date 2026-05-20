import { EventEmitter } from 'events';

export class PaymentService extends EventEmitter {
  constructor(private readonly apiKey: string) {
    super();
  }

  async charge(amount: number): Promise<void> {
    this.emit('charge', amount);
  }
}
