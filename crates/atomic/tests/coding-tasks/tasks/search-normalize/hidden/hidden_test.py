import subprocess
import sys
import unittest

from findit.corpus import load
from findit.index import Index
from findit.search import search


def ids(results):
    return [doc for doc, _ in results]


class SeedDocs(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.index = load("docs")

    def check(self, query, expected):
        self.assertEqual(ids(search(self.index, query)), expected, query)

    def test_accents_and_case(self):
        self.check("Café", ["cafe-terminal", "onboarding", "refunds"])
        self.check("CAFÉ", ["cafe-terminal", "onboarding", "refunds"])
        self.check("résumé", ["resume-jobs", "hiring-resume"])
        self.check("Résumé checkpoint", ["resume-jobs"])
        self.assertEqual(search(self.index, "Café"), search(self.index, "cafe"))

    def test_casefold(self):
        self.check("Straße", ["depot-strasse"])
        self.check("STRASSE", ["depot-strasse"])
        self.check("Lindenstraße", ["depot-strasse"])

    def test_phrases_with_stopwords(self):
        self.check('"point of sale"', ["refunds", "terminal-draft", "cafe-terminal"])
        self.check('"state of the art"', ["monitoring", "hiring-resume"])
        self.check('"Refund at the Café"', ["cafe-terminal"])
        self.check('"Lindenstraße 14"', ["depot-strasse"])

    def test_unquoted_word_with_hyphens(self):
        self.check("state-of-the-art", ["art-wall", "monitoring", "hiring-resume"])

    def test_exclusions(self):
        self.check('refund "point of sale" -draft', ["refunds", "cafe-terminal"])
        self.check("terminal -Café", ["terminal-draft"])
        self.check("résumé -RESUME", [])

    def test_cli(self):
        run = subprocess.run([sys.executable, "-B", "-m", "findit", "--docs", "docs", '"point of sale" -DRAFT'],
                             capture_output=True, text=True, timeout=30)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual([line.split("\t")[0] for line in run.stdout.splitlines()], ["refunds", "cafe-terminal"])


class Inline(unittest.TestCase):
    def setUp(self):
        self.index = Index()
        self.index.add("d1", "The state of the art in caching.")
        self.index.add("d2", "state art")
        self.index.add("d3", "Art of the state.")
        self.index.add("d4", "Upload the ﬁle to ＣＡＦＥ storage.")
        self.index.add("d5", "Naïve Bayes on a résumé")

    def check(self, query, expected):
        self.assertEqual(sorted(ids(search(self.index, query))), sorted(expected), query)

    def test_phrase_positions(self):
        self.check('"state of the art"', ["d1", "d2"])
        self.check('"state art"', ["d1", "d2"])
        self.check('"the art of the state"', ["d3"])
        self.check('"state of the art in caching"', ["d1"])

    def test_compatibility_forms(self):
        self.check("file", ["d4"])
        self.check("ﬁle cafe", ["d4"])
        self.check('"FILE café storage"', ["d4"])

    def test_query_folding(self):
        self.check("NAÏVE", ["d5"])
        self.check('"naive bayes"', ["d5"])
        self.check("RESUME -naïve", [])

    def test_only_stopwords_or_exclusions(self):
        self.check("the of", [])
        self.check("-state", [])
