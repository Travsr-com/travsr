// FROZEN FIXTURE - do not reformat. expected.json pins line numbers.
// Case: zero-caller symbol. Nothing in this fixture calls `main`.
import { handleRequest } from "./handlers";
import { OrderService } from "./orders";

export function main(): void {
  handleRequest(new OrderService());
}
