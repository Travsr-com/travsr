# 479 golden fixture — Python, parsed under a test-module vname path.
import unittest


class CalibrationTests(unittest.TestCase):
    def test_calibrate(self):  # EntryPoint: test_ method in a TestCase subclass.
        self.helper()

    def helper(self):  # Support: helper in the TestCase scope, non-test name.
        pass


def test_module_level():  # EntryPoint: test_ function in a test_*.py file.
    pass
