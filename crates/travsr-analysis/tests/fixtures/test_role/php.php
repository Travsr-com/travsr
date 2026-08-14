<?php
// #479 golden fixture — PHP. #[Test] attribute decisive; `extends TestCase`
// is a Support scope.
use PHPUnit\Framework\TestCase;

class CalibrationTest extends TestCase {
    #[Test]
    public function calibrates() {} // EntryPoint: #[Test] attribute.

    public function helper() {} // Support: helper in the TestCase scope.
}

class Calibrator {
    public function calibrateFloors() {} // None: ordinary production code.

    public function testConnectionPool() {} // Adversarial None: test-ish name, no attr, no scope.
}
