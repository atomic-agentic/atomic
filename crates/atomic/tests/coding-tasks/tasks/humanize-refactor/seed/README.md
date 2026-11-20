# duscan

Reports where the bytes are under a directory. Standard library only, Python 3.10 or later.

```
python3 -m duscan PATH                 # the summary
python3 -m duscan PATH --top 10        # with the ten largest files
python3 -m duscan PATH --tree --depth 2
python3 -m duscan --help               # every option
```

## The summary

```
files: 1.2k  directories: 48  total: 310.4 MiB

largest 5:
    96.0 MiB  media/intro.mov
  ...

by extension:
  .mov          3    250.1 MiB   80.6%
  ...

biggest directories:
   280.0 MiB  media/
```

## Numbers

Scripts read these strings, so their format is part of the interface and does not change between releases.
`duscan/humanize.py` makes all of them:

- `format_bytes(size, precision=1)`: binary units, `B` to `PiB`, as in `1.5 KiB`. Whole bytes print with no
  decimals, as in `512 B`.
- `format_si(count, precision=1)`: counts with an SI prefix and no space, as in `1.5k` and `42`.
- `format_rate(bytes_per_second, precision=1)`: throughput in decimal units, as in `2.5 MB/s`.
- `format_share(part, whole)`: a percentage to one decimal.

Each refuses anything that is not an int or a float, `bool` included, with a `TypeError` that names it.

## Development

```
python3 -m unittest discover -s tests -t .
```
