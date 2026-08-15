// FROZEN FIXTURE - do not reformat. expected.json pins line numbers.
import { OrderId } from "./types";

export class OrderService {
  submit(id: OrderId): boolean {
    return id.value > 0;
  }
}
