// #479 golden fixture — Scala. @Test annotation decisive (scope path-based,
// deferred to Phase 2).
import org.junit.Test

class CalibrationTest {
  @Test
  def calibrates(): Unit = {} // EntryPoint: @Test annotation.

  def helper(): Unit = {} // None in Phase 1.
}

object Calibrator {
  def calibrateFloors(): Unit = {} // None: ordinary production code.

  def testConnectionPool(): Unit = {} // Adversarial None: test-ish name, no annotation.
}
