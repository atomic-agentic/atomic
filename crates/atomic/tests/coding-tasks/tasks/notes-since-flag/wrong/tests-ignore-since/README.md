# notes

A small command-line notebook. Notes live in one JSON file, and the tool lists, searches and exports them.
Standard library only, Python 3.10 or later.

## Usage

```
python3 -m notes [--file PATH] add TITLE [--body TEXT] [--tag TAG]...
python3 -m notes [--file PATH] list [--tag TAG]... [--text WORDS] [--since YYYY-MM-DD] [--limit N]
python3 -m notes [--file PATH] show ID
python3 -m notes [--file PATH] export [--format text|json|csv] [--tag TAG]... [--text WORDS] [--since YYYY-MM-DD]
                               [--output PATH]
python3 -m notes [--file PATH] tags
```

- `--file PATH` names the notebook. Without it, `NOTES_FILE` from the environment is used, and without that,
  `notes.json` in the current directory. A missing notebook is an empty one.
- `add` prints the new note's number. Its created time is now, in UTC. Set `NOTES_NOW` to an ISO timestamp with
  an offset to fix the clock, as the tests do.
- `list` prints one line a note, oldest first: number, created day (UTC), title and tags.
- `--tag TAG` keeps notes that carry the tag, ignoring case. Repeat it to require several tags.
- `--text WORDS` keeps notes whose title or body contains WORDS, ignoring case.
- `--since YYYY-MM-DD` keeps notes created on that day or later, by the created day in UTC.
- `--limit N` keeps the first N notes, after every other filter.
- `export` writes the matching notes in a format: `json` (the default), `csv` or `text`, which is the same as
  `list`. With `--output PATH` it writes to that file instead of standard output.
- `tags` prints each tag with the number of notes that carry it, most used first.

Exit status: 0 on success, 1 when a note is not found or the notebook cannot be read, and 2 for bad usage.

## The notebook file

```json
{"version": 1, "notes": [
  {"id": 1, "title": "Buy paint", "body": "", "tags": ["home"], "created": "2026-03-04T10:15:00Z"}
]}
```

`created` is an ISO 8601 timestamp with an offset (`Z` or `+05:30`). Notes written by `add` are stored in UTC,
but imported notebooks may keep other offsets, and the tool keeps them as they are. Every day the tool shows or
compares is the day in UTC.

## Development

```
python3 -m unittest discover -s tests -t .
```
