import json
import os
import tempfile
import unittest

from notes import storage
from tests.fixtures import sample_notes


class Storage(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.dir.name, "notes.json")

    def tearDown(self):
        self.dir.cleanup()

    def test_missing_is_empty(self):
        self.assertEqual(storage.load(self.path), [])

    def test_save_then_load(self):
        storage.save(self.path, sample_notes())
        self.assertEqual([n.to_dict() for n in storage.load(self.path)], [n.to_dict() for n in sample_notes()])
        self.assertEqual(os.listdir(self.dir.name), ["notes.json"])

    def test_wrong_version(self):
        with open(self.path, "w") as f:
            json.dump({"version": 2, "notes": []}, f)
        with self.assertRaises(storage.StorageError):
            storage.load(self.path)

    def test_duplicate_ids(self):
        note = sample_notes()[0].to_dict()
        with open(self.path, "w") as f:
            json.dump({"version": 1, "notes": [note, note]}, f)
        with self.assertRaises(storage.StorageError):
            storage.load(self.path)

    def test_next_id_and_find(self):
        notes = sample_notes()
        self.assertEqual(storage.next_id(notes), 6)
        self.assertEqual(storage.next_id([]), 1)
        self.assertEqual(storage.find(notes, 4).title, "Read about tide tables")
        self.assertIsNone(storage.find(notes, 9))

    def test_default_path(self):
        self.assertEqual(storage.default_path({}), "notes.json")
        self.assertEqual(storage.default_path({"NOTES_FILE": "/x/y.json"}), "/x/y.json")


if __name__ == "__main__":
    unittest.main()
