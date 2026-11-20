# deployctl

Checks deploy files before they ship. Standard library only, Python 3.10 or later.

```
python3 -m deployctl check conf/prod.conf     # print each problem; exit 1 if there are any
python3 -m deployctl show conf/prod.conf      # print every setting, expanded
sh scripts/check-all.sh                       # check every deploy file in conf/
```

`check` prints `ok: N settings from M files` for a clean file and exits 0. Otherwise it prints one line a problem,
`PATH:LINE: error: MESSAGE`, and exits 1. PATH is relative to the directory of the file being checked when it is
inside it, and absolute otherwise. Bad usage exits 2.

## Deploy files

```
# comments start with # or ;
include = common.conf

[service]
name = storefront-web
image = ${registry}/storefront/web:2.14.1
registry = ${service.registry_host}
```

- `[section]` starts a section, and `key = value` sets a key in it. Values may be in double quotes.
- `include = PATH` reads another deploy file at that point, as if its text were there. **A relative PATH is
  relative to the directory of the file that holds the include line**, not to the directory deployctl is run
  from, so a file in `conf/services/` that includes `limits.conf` reads `conf/services/limits.conf`. `~` is
  expanded, and an absolute PATH is used as it is. Includes may be nested, up to 8 deep. After an include, the
  including file carries on in the section it was in.
- A later setting replaces an earlier one, wherever each came from.
- An include that cannot be found is an error, `include not found: PATH`, with PATH the absolute path that was
  looked for, reported at the include line. An include that leads back to a file that is still being read is an
  error too, `include cycle through PATH`, with PATH the absolute path of that file.
- `${key}` expands to another key of the same section, `${section.key}` to a key of another, and `$$` to `$`.
  Expansion happens after every include is read.

## Rules

`[service]` needs `name` and `image`, and `[deploy]` needs `env` (`dev`, `staging` or `prod`) and `replicas`
(1 to 50, at least 2 in prod). Every `port` and `*_port` is 1 to 65535. An image needs a tag, and prod may not
use `latest`. `[health]` `path` starts with `/`, and `interval` and `timeout` are durations such as `10s`.

## Development

```
python3 -m unittest discover -s tests -t .
```
