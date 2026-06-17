package negative

// Pure Go — no cgo, no FFI markers expected
func PureFunction(x int) int {
    return x + 1
}
