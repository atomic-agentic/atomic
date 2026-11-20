"""The command line: python3 -m notes COMMAND."""

import argparse
import os
import re
import sys
from datetime import date, datetime, timezone

from notes import __version__, storage
from notes.formatters import FORMATS, format_text
from notes.model import Note, NoteError, format_timestamp, parse_timestamp
from notes.query import filter_notes, tag_counts


def positive_int(text):
    try:
        value = int(text)
    except ValueError:
        raise argparse.ArgumentTypeError(f"not a number: {text!r}") from None
    if value < 1:
        raise argparse.ArgumentTypeError(f"must be 1 or more: {value}")
    return value


def iso_date(text):
    if not re.fullmatch(r"\d{4}-\d{2}-\d{2}", text):
        raise argparse.ArgumentTypeError(f"not a date in YYYY-MM-DD form: {text!r}")
    try:
        return date.fromisoformat(text)
    except ValueError:
        raise argparse.ArgumentTypeError(f"not a date: {text!r}") from None


def add_filters(parser):
    parser.add_argument("--tag", action="append", default=[], metavar="TAG",
                        help="keep notes with this tag (repeat to require several)")
    parser.add_argument("--text", metavar="WORDS", help="keep notes whose title or body contains WORDS")
    parser.add_argument("--since", type=iso_date, metavar="YYYY-MM-DD",
                        help="keep notes created on this day (UTC) or later")


def build_parser():
    parser = argparse.ArgumentParser(prog="notes", description="A small command-line notebook.")
    parser.add_argument("--version", action="version", version=f"notes {__version__}")
    parser.add_argument("--file", metavar="PATH", help="the notebook (default: $NOTES_FILE, else notes.json)")
    commands = parser.add_subparsers(dest="command", metavar="COMMAND", required=True)

    add = commands.add_parser("add", help="add a note")
    add.add_argument("title")
    add.add_argument("--body", default="")
    add.add_argument("--tag", action="append", default=[], metavar="TAG")

    listing = commands.add_parser("list", help="list notes, oldest first")
    add_filters(listing)
    listing.add_argument("--limit", type=positive_int, metavar="N", help="keep the first N notes")

    show = commands.add_parser("show", help="print one note")
    show.add_argument("id", type=positive_int)

    export = commands.add_parser("export", help="write notes as JSON, CSV or text")
    add_filters(export)
    export.add_argument("--format", choices=sorted(FORMATS), default="json")
    export.add_argument("--output", metavar="PATH", help="write to PATH instead of standard output")

    commands.add_parser("tags", help="count the notes for each tag")
    return parser


def now(environ):
    fixed = environ.get("NOTES_NOW")
    if fixed:
        return parse_timestamp(fixed)
    return datetime.now(timezone.utc)


def cmd_add(args, notes, path, environ):
    note = Note(id=storage.next_id(notes), title=args.title.strip(), body=args.body, tags=args.tag,
                created=format_timestamp(now(environ)))
    if not note.title:
        print("notes: a note needs a title", file=sys.stderr)
        return 2
    notes.append(note)
    storage.save(path, notes)
    print(note.id)
    return 0


def cmd_list(args, notes, path, environ):
    chosen = filter_notes(notes, tags=args.tag, text=args.text, limit=args.limit, since=args.since)
    sys.stdout.write(format_text(chosen))
    return 0


def cmd_show(args, notes, path, environ):
    note = storage.find(notes, args.id)
    if note is None:
        print(f"notes: no note {args.id}", file=sys.stderr)
        return 1
    print(f"#{note.id} {note.title}")
    print(f"created {note.created}")
    if note.tags:
        print("tags " + ", ".join(note.tags))
    if note.body:
        print()
        print(note.body)
    return 0


def cmd_export(args, notes, path, environ):
    chosen = filter_notes(notes, tags=args.tag, text=args.text, since=args.since)
    text = FORMATS[args.format](chosen)
    if args.output:
        with open(args.output, "w", encoding="utf-8", newline="") as f:
            f.write(text)
    else:
        sys.stdout.write(text)
    return 0


def cmd_tags(args, notes, path, environ):
    for tag, count in tag_counts(notes):
        print(f"{count:>4}  {tag}")
    return 0


COMMANDS = {"add": cmd_add, "list": cmd_list, "show": cmd_show, "export": cmd_export, "tags": cmd_tags}


def main(argv=None, environ=None):
    environ = os.environ if environ is None else environ
    args = build_parser().parse_args(argv)
    path = args.file or storage.default_path(environ)
    try:
        notes = storage.load(path)
        return COMMANDS[args.command](args, notes, path, environ)
    except (storage.StorageError, NoteError) as error:
        print(f"notes: {error}", file=sys.stderr)
        return 1
