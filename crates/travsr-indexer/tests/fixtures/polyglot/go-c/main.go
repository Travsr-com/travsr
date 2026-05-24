package main

/*
#include "helpers.h"
*/
import "C"
import "fmt"

func main() {
    result := C.add(1, 2)
    fmt.Println(result)
}

//export GoCallback
func GoCallback(x C.int) C.int {
    return x * 2
}
