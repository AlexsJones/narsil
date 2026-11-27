import unittest

from tideline.table import render


class RenderTest(unittest.TestCase):
    def test_one_row_for_each_tide(self):
        tides = [{"time": "04:12", "kind": "high", "height": 5.3}, {"time": "10:31", "kind": "low", "height": 0.9}]
        self.assertEqual(render(tides), "04:12  high   5.3 m\n10:31  low    0.9 m")


if __name__ == "__main__":
    unittest.main()
