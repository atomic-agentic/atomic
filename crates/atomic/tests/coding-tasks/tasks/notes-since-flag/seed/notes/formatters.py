"""Turning notes into text: the line `list` prints, JSON and CSV."""

import csv
import io
import json


def text_line(note):
    tags = " ".join(f"#{t}" for t in note.tags)
    line = f"{note.id:>4}  {note.created_day.isoformat()}  {note.title}"
    return f"{line}  {tags}" if tags else line


def format_text(notes):
    return "".join(text_line(n) + "\n" for n in notes)


def format_json(notes):
    return json.dumps([n.to_dict() for n in notes], indent=2, ensure_ascii=False) + "\n"


def format_csv(notes):
    out = io.StringIO()
    writer = csv.writer(out, lineterminator="\n")
    writer.writerow(["id", "created", "title", "tags", "body"])
    for note in notes:
        writer.writerow([note.id, note.created, note.title, ";".join(note.tags), note.body])
    return out.getvalue()


FORMATS = {"text": format_text, "json": format_json, "csv": format_csv}
