# verrange

verrange matches semantic versions against version ranges, for the release tooling
that picks which build of each internal package a deployment gets.

    python3 -m verrange '^1.2.0 || ~2.0.1' 1.1.9 1.4.0 2.0.7 2.1.0
    2.0.7

prints the highest version that satisfies the range. `--all` prints every one that
does, lowest first. With none, it prints `no version satisfies RANGE` on stderr and
exits 1. A version or range it cannot parse is reported the same way, with exit 2.

## Versions

`MAJOR.MINOR.PATCH`, with an optional pre-release after `-` and optional build data
after `+`, as in `1.4.0-rc.2+build.77`. A leading `v` is allowed. Major, minor and patch
are non-negative integers. The pre-release and the build are dot-separated identifiers
of ASCII letters, digits and `-`.

Precedence (which version is lower):

1. Major, minor and patch are compared as numbers, in that order.
2. When they are equal, a version with a pre-release is lower than one without:
   `1.0.0-rc.1 < 1.0.0`.
3. Two pre-releases are compared identifier by identifier, from the left:
   - identifiers made only of digits are compared as numbers: `rc.9 < rc.10`;
   - other identifiers are compared as ASCII strings: `alpha < beta`;
   - a digits-only identifier is lower than any other: `alpha.1 < alpha.beta`;
   - if every identifier is equal so far, the one with fewer is lower: `alpha < alpha.1`.
4. Build data is ignored: `1.0.0+a` and `1.0.0+b` are equal.

So: `1.0.0-alpha < 1.0.0-alpha.1 < 1.0.0-alpha.beta < 1.0.0-beta < 1.0.0-beta.2 <
1.0.0-beta.11 < 1.0.0-rc.1 < 1.0.0`.

## Ranges

A range is one or more **comparator sets** joined by `||`. A version satisfies the
range if it satisfies any set. A set is one or more comparators separated by spaces,
and a version satisfies the set if it satisfies every comparator in it. An empty set
means any version.

Comparators:

| Written | Means |
|---|---|
| `1.2.3` or `=1.2.3` | exactly 1.2.3 |
| `>1.2.3`, `>=1.2.3`, `<1.2.3`, `<=1.2.3` | as written; these need a full version |
| `*`, `x` | any version (`>=0.0.0`) |
| `1.x`, `1.*`, `1` | `>=1.0.0 <2.0.0` |
| `1.2.x`, `1.2` | `>=1.2.0 <1.3.0` |
| `~1.2.3` | `>=1.2.3 <1.3.0`: patch updates only |
| `~1.2` | `>=1.2.0 <1.3.0` |
| `~1` | `>=1.0.0 <2.0.0` |
| `^1.2.3` | `>=1.2.3 <2.0.0`: anything that does not change the first non-zero part |
| `^0.2.3` | `>=0.2.3 <0.3.0` |
| `^0.0.3` | `>=0.0.3 <0.0.4` |
| `^1.2`, `^1` | `>=1.2.0 <2.0.0`, `>=1.0.0 <2.0.0` |
| `^0.2` | `>=0.2.0 <0.3.0` |
| `^0.0`, `^0` | `>=0.0.0 <0.1.0`, `>=0.0.0 <1.0.0` |

A space may follow `<`, `>`, `<=`, `>=` and `=`: `>= 1.2.3` is `>=1.2.3`. `~` and `^`
also take a pre-release: `^1.2.3-beta.2` is `>=1.2.3-beta.2 <2.0.0`.

### Pre-releases

A version with a pre-release satisfies a comparator set only if it satisfies every
comparator **and** at least one comparator in the set names a pre-release of the same
major, minor and patch. So `1.3.0-rc.1` does not satisfy `^1.2.0`, but `1.2.4-beta.1`
satisfies `>=1.2.4-alpha <1.3.0`, and `1.2.5-beta.1` does not.

## Code

- `verrange/version.py`: parsing and precedence.
- `verrange/comparator.py`: one comparator.
- `verrange/ranges.py`: parsing ranges, and `satisfies(version, range)`.
- `verrange/resolve.py`: `max_satisfying`, `min_satisfying` and `all_satisfying`.
- `verrange/cli.py`: the command line.

## Tests

    python3 -m unittest discover -s tests -t .
