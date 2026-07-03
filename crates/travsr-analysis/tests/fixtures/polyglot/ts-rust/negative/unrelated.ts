// Not in a napi package — must produce 0 FFI markers
export function notAnFfiFunction(x: number): number {
    return x * 2;
}
