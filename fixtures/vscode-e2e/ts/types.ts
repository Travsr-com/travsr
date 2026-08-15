// FROZEN FIXTURE - do not reformat. expected.json pins line numbers.
// Case: file with no function or method nodes. CodeLens must render nothing
// here rather than a "0 callers" lens on a type declaration.
export interface OrderId {
  value: number;
}

export type OrderStatus = "pending" | "settled";
