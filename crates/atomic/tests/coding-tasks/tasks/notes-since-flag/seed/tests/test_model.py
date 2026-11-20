import unittest
from datetime import date, timezone

from notes.model import Note, NoteError, format_timestamp, parse_timestamp


class Timestamps(unittest.TestCase):
    def test_z_and_offsets(self):
        self.assertEqual(parse_timestamp("2026-03-04T10:15:00Z").utcoffset().total_seconds(), 0)
        moment = parse_timestamp("2026-03-04T10:15:00+05:30")
        self.assertEqual(format_timestamp(moment), "2026-03-04T04:45:00Z")

    def test_offset_required(self):
        with self.assertRaises(NoteError):
            parse_timestamp("2026-03-04T10:15:00")
        with self.assertRaises(NoteError):
            parse_timestamp("yesterday")

    def test_created_day_is_utc(self):
        note = Note(1, "t", created="2026-03-04T23:30:00-02:00")
        self.assertEqual(note.created_day, date(2026, 3, 5))
        self.assertEqual(note.created_at.astimezone(timezone.utc).hour, 1)


class FromDict(unittest.TestCase):
    def test_round_trip(self):
        data = {"id": 3, "title": "x", "body": "y", "tags": ["a"], "created": "2026-01-01T00:00:00Z"}
        self.assertEqual(Note.from_dict(data).to_dict(), data)

    def test_rejects_bad_records(self):
        for bad in ({"title": "x", "created": "2026-01-01T00:00:00Z"},
                    {"id": 0, "title": "x", "created": "2026-01-01T00:00:00Z"},
                    {"id": 1, "title": " ", "created": "2026-01-01T00:00:00Z"},
                    {"id": 1, "title": "x", "tags": "a", "created": "2026-01-01T00:00:00Z"},
                    {"id": 1, "title": "x", "created": "soon"}):
            with self.assertRaises(NoteError):
                Note.from_dict(bad)


if __name__ == "__main__":
    unittest.main()
