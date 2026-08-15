// FROZEN FIXTURE - do not reformat. expected.json pins line numbers.
import { OrderService } from "./orders";

export function nightlyJob(svc: OrderService): boolean {
  return svc.submit({ value: 2 });
}
