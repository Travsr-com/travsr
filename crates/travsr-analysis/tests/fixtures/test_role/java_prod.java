// #479 golden fixture — Java, parsed under a src/main/java vname path.
package com.foo;

class Calibrator {
    void calibrateFloors() {} // None: ordinary production code.

    // Adversarial None: a test-ish name with no @Test annotation, production path.
    void testConnectionPool() {}
}
