# logship

logship turns application log records into syslog-style lines and hands them to a
collector. It reads records as JSON lines, formats each one, checks it the way the
collector will, and writes the lines to a file that the shipping agent tails.

## Running it

    python3 -m logship --config examples/logship.conf examples/records.jsonl

Options:

- `--config FILE`: a configuration file (see below). Without one the defaults apply.
- `--output FILE`: where to write the lines. Overrides `output` in the configuration.

It prints `sent N lines, collector rejected M` and, for each rejected line,
`rejected line K: REASON` on stderr. It exits 0 when every line was accepted and 1
when any was rejected.

## Records

One JSON object per line:

    {"time": 1767225600, "severity": "info", "message": "cache warmed"}

- `time`: seconds since the epoch (UTC), an integer.
- `severity`: one of `emerg`, `alert`, `crit`, `err`, `warning`, `notice`, `info`, `debug`.
- `message`: the text.
- `host`, `app`: optional; they override the configured ones for this record.

## Lines

Each record becomes one line:

    <PRI>TIMESTAMP HOST APP: MESSAGE

- `PRI` is `facility * 8 + severity`, with `severity` numbered 0 (`emerg`) to 7 (`debug`).
- `TIMESTAMP` is the time in UTC, as `YYYY-MM-DDTHH:MM:SSZ`.
- `HOST` and `APP` have any whitespace replaced by `-`; an empty one is written as `-`.
- In `MESSAGE`, every run of control characters (newlines, tabs and the like) becomes
  one space, and leading and trailing spaces are removed.

Host names, app names and messages may hold any Unicode text.

### The line limit

The collector accepts a line only if it is valid UTF-8 and, encoded as UTF-8, at most
`max_line_bytes` bytes long (the newline that ends it is not counted). So every line
logship writes must meet that limit:

- A line that fits is written as it is.
- A line that does not fit keeps its whole prefix (`<PRI>TIMESTAMP HOST APP: `), and
  its message is cut so that the line, with `...` (three ASCII dots) added at the end,
  fits. The message is cut between characters (Unicode code points), never inside one,
  and keeps as many whole characters from its start as will fit. Nothing else is
  removed: a space just before the cut stays.
- If the prefix and `...` alone do not fit, the record cannot be sent, and logship
  raises `ValueError`.

## Configuration

A file of `key = value` lines. Blank lines and lines starting with `#` are ignored.

| Key | Default | Meaning |
|---|---|---|
| `host` | `localhost` | the host name written in each line |
| `app` | `logship` | the app name written in each line |
| `facility` | `1` | the syslog facility, 0-23 |
| `max_line_bytes` | `120` | the collector's limit, in bytes |
| `output` | `out.log` | where the lines go |

An unknown key, or a value of the wrong kind, is an error.

## Tests

    python3 -m unittest discover -s tests -t .
