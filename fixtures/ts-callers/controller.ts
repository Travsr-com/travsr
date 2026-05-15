import { PaymentService } from "./service";

function processPayment(svc: PaymentService) {
  return svc.charge(100);
}
