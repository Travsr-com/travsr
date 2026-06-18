// Package simple is a minimal Go fixture for the travsr indexer bench.
package simple

import "fmt"

const Version = "1.0.0"

var DefaultTimeout = 30

type Config struct {
	Host string
	Port int
}

type Runner interface {
	Run() error
}

func New(host string, port int) *Config {
	return &Config{Host: host, Port: port}
}

func (c *Config) Run() error {
	fmt.Printf("running on %s:%d\n", c.Host, c.Port)
	return nil
}

func (c Config) String() string {
	return fmt.Sprintf("%s:%d", c.Host, c.Port)
}
