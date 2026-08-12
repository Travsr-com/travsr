// #479 golden fixture — Java, parsed under a src/test/java vname path.
package com.foo;

import org.junit.jupiter.api.Test;

class CalibrationTest {
    @Test
    void calibrates() {} // EntryPoint: @Test annotation (decisive).

    void helper() {} // Support: helper in the src/test scope, no annotation.
}
