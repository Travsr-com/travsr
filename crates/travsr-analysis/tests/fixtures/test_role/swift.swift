// #479 golden fixture — Swift. swift-testing @Test decisive; XCTestCase
// subclass is a Support scope.
import Testing
import XCTest

@Test func calibrates() {} // EntryPoint: swift-testing @Test.

class CalibrationTests: XCTestCase {
    func testCalibrate() {} // Support: method in the XCTestCase scope.
    func helper() {} // Support: helper in the XCTestCase scope.
}

func calibrateFloors() {} // None: ordinary production code.

func testConnectionPool() {} // Adversarial None: test-ish name, no @Test, no scope.
