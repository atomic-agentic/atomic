# backup

Two small POSIX `sh` scripts for taking a plain copy of a directory and putting it back.

    sh backup.sh [-a] [-q] [-v] [-l LABEL] SRC DEST
    sh restore.sh [-q] BACKUP TARGET

`backup.sh` copies every regular file under `SRC` into `DEST`, keeping its path, mode and times, and writes
`DEST/MANIFEST`. `DEST` must not exist yet, or be an empty directory. `restore.sh` checks a backup against
its `MANIFEST` and copies the files into `TARGET`, which must be new or empty too. Nothing but `sh` and the
usual POSIX tools is needed.

## Options

| Option | What it does |
|---|---|
| `-a` | Include dot files and everything under dot directories. Without it they are left out. |
| `-l LABEL` | The label on the first line of `MANIFEST`. The default is `backup`. |
| `-q` | Print nothing when the backup works. |
| `-v` | Say each file as it is copied, on stderr. |
| `-h` | Print the usage and exit 0. |

Options come before `SRC` and `DEST`, and `--` ends them. A missing or unknown option argument is a usage
error: the usage goes to stderr and the exit status is 2. Any other failure exits 1.

## MANIFEST

    label: nightly
    source: /srv/site
    files: 3
    dirs: 2
    bytes: 1874
    --
    index.html
    img/logo.png
    img/photo one.jpg

- `source` is `SRC` as given, without a trailing slash.
- `files` is the number of files copied, and `bytes` their total size.
- `dirs` is the number of directories that hold at least one copied file directly, counting `SRC` itself
  as `.`.
- After `--` come the copied paths, relative to `SRC`, one a line, sorted bytewise (`LC_ALL=C`).

`restore.sh` refuses a backup whose counts do not match its paths and files.

## Limits

File names may hold spaces and any other character except a newline. Links, devices and empty directories
are not copied.

## Tests

    sh tests/run.sh
