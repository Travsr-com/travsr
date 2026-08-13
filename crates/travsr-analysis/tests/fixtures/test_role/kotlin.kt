// #479 golden fixture — Kotlin. @Test annotation is decisive (scope is
// path-based, deferred to Phase 2, so no Support case here).
import kotlin.test.Test

class CalibrationTest {
    @Test
    fun calibrates() {} // EntryPoint: @Test annotation.

    fun helper() {} // None in Phase 1 (no annotation, path-based scope deferred).
}

fun calibrateFloors() {} // None: ordinary production code.

fun testConnectionPool() {} // Adversarial None: test-ish name, no annotation.
