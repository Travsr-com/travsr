// #479 golden fixture — Go, parsed under a `_test.go` vname path.
package foo

import "testing"

// EntryPoint: a TestX with a *testing.T parameter, in a _test.go file.
func TestCalibrate(t *testing.T) {
	helper()
}

// Support: a helper inside the _test.go scope (no testing param, plain name).
func helper() {}
