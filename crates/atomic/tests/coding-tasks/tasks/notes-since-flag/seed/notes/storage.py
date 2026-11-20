"""Reading and writing the notebook file."""

import json
import os
import tempfile

from notes.model import Note, NoteError

VERSION = 1


class StorageError(Exception):
    """The notebook cannot be read or written."""


def default_path(environ=None):
    environ = os.environ if environ is None else environ
    return environ.get("NOTES_FILE") or "notes.json"


def load(path):
    """Every note in the notebook at path. A missing notebook is empty."""
    try:
        with open(path, encoding="utf-8") as f:
            data = json.load(f)
    except FileNotFoundError:
        return []
    except (OSError, ValueError) as error:
        raise StorageError(f"cannot read {path}: {error}") from None
    if not isinstance(data, dict) or data.get("version") != VERSION:
        raise StorageError(f"{path} is not a version {VERSION} notebook")
    notes = []
    seen = set()
    for item in data.get("notes", []):
        try:
            note = Note.from_dict(item)
        except NoteError as error:
            raise StorageError(f"{path}: {error}") from None
        if note.id in seen:
            raise StorageError(f"{path}: note {note.id} appears twice")
        seen.add(note.id)
        notes.append(note)
    return notes


def save(path, notes):
    """Writes the notebook whole, through a temporary file, so a failed write leaves the old one."""
    directory = os.path.dirname(os.path.abspath(path))
    payload = {"version": VERSION, "notes": [n.to_dict() for n in sorted(notes, key=lambda n: n.id)]}
    fd, temporary = tempfile.mkstemp(prefix=".notes-", dir=directory)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(payload, f, indent=1, ensure_ascii=False)
            f.write("\n")
        os.replace(temporary, path)
    except BaseException:
        if os.path.exists(temporary):
            os.unlink(temporary)
        raise


def next_id(notes):
    return max((n.id for n in notes), default=0) + 1


def find(notes, note_id):
    for note in notes:
        if note.id == note_id:
            return note
    return None
