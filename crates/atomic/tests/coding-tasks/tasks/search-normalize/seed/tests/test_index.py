import unittest

from findit.index import Index


class IndexTest(unittest.TestCase):
    def setUp(self):
        self.index = Index()
        self.index.add("one", "Refund the card. Refund again.")
        self.index.add("two", "The card was declined.")

    def test_postings(self):
        self.assertEqual(self.index.positions("refund", "one"), [0, 2])
        self.assertEqual(self.index.docs_with("card"), {"one", "two"})
        self.assertEqual(self.index.doc_freq("refund"), 1)
        self.assertEqual(self.index.term_freq("refund", "one"), 2)
        self.assertEqual(self.index.positions("missing", "one"), [])

    def test_lengths(self):
        self.assertEqual(self.index.length("one"), 4)
        self.assertEqual(self.index.length("two"), 2)
        self.assertEqual(len(self.index), 2)
        self.assertEqual(self.index.doc_ids, ["one", "two"])

    def test_duplicate_id(self):
        with self.assertRaises(ValueError):
            self.index.add("one", "again")
