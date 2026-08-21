// FROZEN FIXTURE - do not reformat. expected.json pins line numbers.
import { OrderService } from "./orders";
import { format } from "./util";

export function handleRequest(svc: OrderService): string {
  const ok = svc.submit({ value: 1 });
  return format(ok ? 1 : 0);
}
