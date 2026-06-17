package main

import (
	"fmt"
	"os"

	"github.com/example/go-small/handler"
)

var Version = "0.1.0"

const AppName = "go-small"

func main() {
	h := handler.New()
	fmt.Fprintln(os.Stdout, h.Name())
}

func run(args []string) error {
	_ = args
	return nil
}
