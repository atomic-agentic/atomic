# Command-line options. parse_args sets SRC, DEST, LABEL, ALL, VERBOSE, QUIET, EXCLUDES and EXCLUDING,
# or exits 2. Options come before SRC and DEST; -- ends them.

LABEL=backup
ALL=0
SRC=
DEST=
# The -x patterns, one a line, and whether any -x was given.
EXCLUDES=
EXCLUDING=0
NL='
'

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            -a)
                ALL=1
                shift
                ;;
            -q)
                QUIET=1
                shift
                ;;
            -v)
                VERBOSE=1
                shift
                ;;
            -h)
                usage
                exit 0
                ;;
            -l)
                [ $# -ge 2 ] || usage_error "-l needs a label"
                LABEL=$2
                shift 2
                ;;
            -x)
                [ $# -ge 2 ] || usage_error "-x needs a pattern"
                case "$2" in
                    *"$NL"*) usage_error "a pattern is one line" ;;
                esac
                if [ "$EXCLUDING" -eq 1 ]; then
                    EXCLUDES=$EXCLUDES$NL$2
                else
                    EXCLUDES=$2
                fi
                EXCLUDING=1
                shift 2
                ;;
            --)
                shift
                break
                ;;
            -?*)
                usage_error "unknown option $1"
                ;;
            *)
                break
                ;;
        esac
    done
    [ $# -eq 2 ] || usage_error "give SRC and DEST"
    case "$LABEL" in
        "" | *"
"*) usage_error "a label is one line, and not empty" ;;
    esac
    SRC=$1
    DEST=$2
    # Without the trailing slash, so that paths join cleanly; / stays /.
    while [ "${SRC%/}" != "$SRC" ] && [ "$SRC" != / ]; do SRC=${SRC%/}; done
    while [ "${DEST%/}" != "$DEST" ] && [ "$DEST" != / ]; do DEST=${DEST%/}; done
}
