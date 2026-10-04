package com.acme.app;

import java.util.List;

public interface Greeter {
    String greet(String name);
}

class Hello implements Greeter {
    private final String prefix;

    Hello(String prefix) {
        this.prefix = prefix;
    }

    @Override
    public String greet(String name) {
        return prefix + " " + name;
    }
}
