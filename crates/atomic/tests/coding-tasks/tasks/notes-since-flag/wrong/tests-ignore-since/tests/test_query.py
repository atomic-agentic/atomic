from datetime import date
import unittest

from notes.query import filter_notes, matches_text, tag_counts
from tests.fixtures import sample_notes


def ids(notes):
    return [n.id for n in notes]


class FilterNotes(unittest.TestCase):
    def test_no_filters_is_everything_oldest_first(self):
        self.assertEqual(ids(filter_notes(sample_notes())), [1, 3, 2, 4, 5])

    def test_tag_ignores_case(self):
        self.assertEqual(ids(filter_notes(sample_notes(), tags=["URGENT"])), [2, 5])

    def test_several_tags_must_all_match(self):
        self.assertEqual(ids(filter_notes(sample_notes(), tags=["work", "urgent"])), [5])

    def test_text_searches_title_and_body(self):
        self.assertEqual(ids(filter_notes(sample_notes(), text="plan")), [3, 5])

    def test_limit_applies_after_filters(self):
        self.assertEqual(ids(filter_notes(sample_notes(), tags=["work"], limit=1)), [3])

    def test_nothing_matches(self):
        self.assertEqual(filter_notes(sample_notes(), text="zebra"), [])


class Helpers(unittest.TestCase):
    def test_matches_text(self):
        note = sample_notes()[0]
        self.assertTrue(matches_text(note, "PAINT"))
        self.assertTrue(matches_text(note, "two litres"))
        self.assertFalse(matches_text(note, "brush"))

    def test_tag_counts(self):
        self.assertEqual(tag_counts(sample_notes()), [("home", 2), ("urgent", 2), ("work", 2)])


if __name__ == "__main__":
    unittest.main()


class Since(unittest.TestCase):
    def test_since_is_accepted(self):
        filter_notes(sample_notes(), since=date(2026, 3, 2))
