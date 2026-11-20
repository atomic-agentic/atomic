import math
import os
import subprocess
import sys
import unittest

from findit.corpus import load
from findit.index import Index
from findit.query import parse
from findit.search import search

DOCS = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "docs")


def ids(results):
    return [doc for doc, _ in results]


class QueryTest(unittest.TestCase):
    def test_parse(self):
        query = parse('refund "batch job" -draft')
        self.assertEqual(query.terms, ["refund"])
        self.assertEqual([[term for _, term in p] for p in query.phrases], [["batch", "job"]])
        self.assertEqual(query.excluded, ["draft"])

    def test_empty(self):
        self.assertTrue(parse("the -draft").is_empty())


class SearchTest(unittest.TestCase):
    def setUp(self):
        self.index = Index()
        self.index.add("a", "deploy the service")
        self.index.add("b", "deploy deploy rollback")
        self.index.add("c", "rollback plan")

    def test_words_are_all_required(self):
        self.assertEqual(ids(search(self.index, "deploy rollback")), ["b"])

    def test_ranking(self):
        results = search(self.index, "deploy")
        self.assertEqual(ids(results), ["b", "a"])
        self.assertAlmostEqual(results[0][1], (2 / 3) * math.log(1 + 3 / 2))

    def test_exclusion(self):
        self.assertEqual(ids(search(self.index, "rollback -deploy")), ["c"])

    def test_phrase(self):
        self.assertEqual(ids(search(self.index, '"rollback plan"')), ["c"])
        self.assertEqual(ids(search(self.index, '"plan rollback"')), [])

    def test_nothing_to_look_for(self):
        self.assertEqual(search(self.index, "-deploy"), [])


class CorpusTest(unittest.TestCase):
    def test_folder(self):
        index = load(DOCS)
        self.assertIn("refunds", index.doc_ids)
        self.assertEqual(ids(search(index, "heartbeat")), ["monitoring"])
        self.assertEqual(ids(search(index, "checkpoint")), ["resume-jobs"])

    def test_cli(self):
        root = os.path.dirname(DOCS)
        run = subprocess.run([sys.executable, "-B", "-m", "findit", "--docs", DOCS, "yard"], cwd=root,
                             capture_output=True, text=True, timeout=30)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(run.stdout.split("\t")[0], "depot-strasse")
        run = subprocess.run([sys.executable, "-B", "-m", "findit", "--docs", DOCS, "zebra"], cwd=root,
                             capture_output=True, text=True, timeout=30)
        self.assertEqual((run.returncode, run.stderr.strip()), (1, "no matches"))
