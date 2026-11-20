import unittest

from findit.analyzer import analyze, terms
from findit.normalize import fold
from findit.tokenize import words


class FoldTest(unittest.TestCase):
    def test_fold(self):
        self.assertEqual(fold("Café"), "cafe")
        self.assertEqual(fold("CAFE"), "cafe")
        self.assertEqual(fold("résumé"), "resume")
        self.assertEqual(fold("Straße"), "strasse")
        self.assertEqual(fold("é"), "e")


class WordsTest(unittest.TestCase):
    def test_split(self):
        self.assertEqual(words("state-of-the-art"), ["state", "of", "the", "art"])
        self.assertEqual(words("don't stop_now"), ["don", "t", "stop", "now"])
        self.assertEqual(words("15:00, 7:00"), ["15", "00", "7", "00"])
        self.assertEqual(words(""), [])

    def test_combining_marks_stay(self):
        self.assertEqual(words("résumé now"), ["résumé", "now"])


class AnalyzerTest(unittest.TestCase):
    def test_terms_drop_stopwords(self):
        self.assertEqual(terms("The state of the Art"), ["state", "art"])

    def test_positions_skip_stopwords(self):
        self.assertEqual(analyze("a refund at the Café"), [(0, "refund"), (1, "cafe")])
