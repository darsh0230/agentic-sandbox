import unittest

from calculator import divide


class DivideTests(unittest.TestCase):
    def test_divides_two_numbers(self) -> None:
        self.assertEqual(divide(12, 3), 4)

    def test_rejects_zero_divisor(self) -> None:
        with self.assertRaises(ZeroDivisionError):
            divide(12, 0)


if __name__ == "__main__":
    unittest.main()

