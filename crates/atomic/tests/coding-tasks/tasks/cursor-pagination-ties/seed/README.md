# tickets

A small help-desk ticket store with a paginated listing API, and a command line that exports tickets
through that API, page by page, as a remote client would.

## Usage

```
python3 -m tickets count data/tickets.json                  how many tickets, by status
python3 -m tickets show data/tickets.json 1042              one ticket
python3 -m tickets export data/tickets.json                 every ticket, as CSV on stdout
python3 -m tickets export --format jsonl --status open --page-size 10 data/tickets.json
```

`export` options: `--format csv|jsonl` (default `csv`), `--status STATUS` (only tickets with that
status), and `--page-size N` (default 25), the page size it asks the API for.

## The listing API

`tickets.api.list_tickets(store, page_size=25, cursor=None, status=None)` returns a `Page` with
`items`, a list of tickets, and `next_cursor`.

- Tickets come in order of `updated_at`, oldest first, and by `id` among tickets with the same
  `updated_at`.
- `page_size` is a whole number from 1 to 100. Anything else raises `ValueError`.
- `next_cursor` is a string to pass as `cursor` for the next page, or `None` on the last page. Every
  page but the last is full.
- Following the cursors from the first page to the last gives every ticket that matches exactly once.
- A cursor is opaque: clients must not read or build one. A cursor that the API did not make raises
  `ValueError`. Cursors are only good for the version of the code that made them.

`updated_at` is UTC, written `YYYY-MM-DDTHH:MM:SSZ`, and many tickets can share one: a triage sweep
updates dozens in the same second.

## Data

`data/tickets.json` is a list of tickets, each with `id` (a positive whole number, unique), `title`,
`status` (`open`, `pending` or `closed`) and `updated_at`.

## Tests

```
python3 -m unittest discover -s tests -t .
```
