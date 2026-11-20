import unittest

from pricer.cache import clear_all, memoize


class MemoizeTest(unittest.TestCase):
    def setUp(self):
        self.calls = []

        @memoize
        def square(x, y=0):
            self.calls.append((x, y))
            return x * x + y

        self.square = square

    def test_caches_by_argument(self):
        self.assertEqual(self.square(3), 9)
        self.assertEqual(self.square(3), 9)
        self.assertEqual(self.square(4), 16)
        self.assertEqual(self.calls, [(3, 0), (4, 0)])
        self.assertEqual(self.square.cache_info(), {"hits": 1, "misses": 2, "size": 2})

    def test_keyword_and_positional_kept_apart(self):
        self.assertEqual(self.square(2, 1), 5)
        self.assertEqual(self.square(2, y=1), 5)
        self.assertEqual(len(self.calls), 2)

    def test_clear(self):
        self.square(5)
        clear_all()
        self.square(5)
        self.assertEqual(self.calls, [(5, 0), (5, 0)])
