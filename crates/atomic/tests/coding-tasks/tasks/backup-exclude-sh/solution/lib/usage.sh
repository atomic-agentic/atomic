# The help text, for -h and for usage errors.

usage() {
    cat <<'EOF'
usage: sh backup.sh [-a] [-q] [-v] [-l LABEL] [-x PATTERN]... SRC DEST

Copy every regular file under SRC into DEST, which must be new or empty, keeping
their paths, and write DEST/MANIFEST, which lists what was copied.

options:
  -a          include dot files and dot directories, which are left out by default
  -l LABEL    the label at the top of MANIFEST (default: backup)
  -q          print nothing when it works
  -v          say what is copied, on stderr
  -x PATTERN  leave out files whose path under SRC matches PATTERN, a shell
              pattern in which * also matches /; may be given more than once
  -h          show this help
EOF
}
