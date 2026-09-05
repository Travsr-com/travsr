<?php

namespace Acme\App;

interface Greeter
{
    public function greet(string $name): string;
}

class Hello implements Greeter
{
    public function __construct(private string $prefix)
    {
    }

    public function greet(string $name): string
    {
        return $this->prefix . ' ' . $name;
    }
}
