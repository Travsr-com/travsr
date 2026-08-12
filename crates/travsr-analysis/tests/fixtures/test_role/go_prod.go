// #479 golden fixture — Go, parsed under a production (non-`_test.go`) path.
package foo

// None: ordinary production code.
func CalibrateFloors() {}

// Adversarial None: a test-ish name in a production file. `go test` only runs
// tests from `_test.go` files, so this is never a test entry point.
func BenchmarkServer() {}
