# 479 golden fixture — Ruby. A `< Minitest::Test` (or Test::Unit) subclass is a
# Support scope; `def test_*` inside it is the EntryPoint.
require "minitest/autorun"

class CalibrationTest < Minitest::Test
  def test_calibrate # EntryPoint: test_ method in a Minitest scope.
  end

  def helper # Support: helper in the Minitest scope.
  end
end

class Calibrator
  def calibrate_floors # None: ordinary production code.
  end

  def test_connection_pool # Adversarial None: test-ish name, no Minitest base.
  end
end
