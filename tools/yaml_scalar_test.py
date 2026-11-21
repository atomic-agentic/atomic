#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Tests for `tools/yaml_scalar.py`, the loader's scalar classifier ported to Python.

`tools/check_examples.py` reads every schema sidecar through this port, so the port
is what decides whether an example is one atomic's loader would take. Two ways it
can be wrong matter most, and both are tested here rather than left to whatever a
YAML library constructs:

  * it reads a spelling as the loader does not -- `0o17` as the string PyYAML
    resolves instead of the 15 the core schema reads, `1e3` as a string instead of
    1000.0, `0200` as the octal 128 YAML 1.1 reads instead of refusing it;
  * it lets through a spelling the loader refuses, which is how a catalog example
    that atomic will not load keeps passing the check that exists to catch it.

So each case says what the text must be read as -- the null, boolean or number the
core schema reads -- or how it must be refused, and every refusal case carries the
reason and the fix the loader's own message carries. A case is refused-or-read,
never merely "did not raise": a port that stopped refusing would fail a case rather
than report one fewer check.

The cases are the same spellings atomic's loader pins in
`crates/atomic-schema/src/yaml/tests.rs`, so the two halves of the mirror move
together. What that file pins for values -- the leading zero, the sign on either
side of a base marker, the numbers only YAML 1.1 reads, what JSON cannot hold, the
base integer past 64 bits and the core schema's scalars -- is what is asserted here.

Nothing here needs the YAML library that reads the catalog, so this runs in the
development image:

    python3 tools/yaml_scalar_test.py

`crates/atomic/tests/catalog_checker.rs` runs it as a cargo test too, which is what
lets a row in `scripts/controls/` switch one of these rules off and show that a test
catches it.
"""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import yaml_scalar as core

failures: list[str] = []
cases = 0

VALUE, REFUSED, TEXT = core.VALUE, core.REFUSED, core.TEXT


def check(what, got, want):
    """One text, and what the port must say about it.

    `want` is `(VALUE, the value)`, `(REFUSED, the reason, the fix)` or `(TEXT,)`.
    A refusal is compared on both halves of its message, because a message that
    says only that a spelling is odd is not one an author can act on.
    """
    global cases
    cases += 1
    kind, answer = core.core_scalar(what)
    if want[0] == REFUSED:
        if kind != REFUSED:
            failures.append(f"{what!r}: read as {kind} {answer!r}, must be refused ({want[1]!r})")
            return
        if answer.problem != want[1]:
            failures.append(f"{what!r}: refused as {answer.problem!r}, not {want[1]!r}")
            return
        if answer.fix != want[2]:
            failures.append(f"{what!r}: fixed as {answer.fix!r}, not {want[2]!r}")
            return
        # The value says both the difference and the fix; the key says only that
        # it must be quoted, which is the fix a key has.
        if answer.as_value() != f"{want[1]}; {want[2]}":
            failures.append(f"{what!r}: the value message is {answer.as_value()!r}")
            return
        if answer.as_key() != f"{want[1]}; quote it":
            failures.append(f"{what!r}: the key message is {answer.as_key()!r}")
            return
        return
    if kind != want[0]:
        failures.append(f"{what!r}: read as {kind} {answer!r}, must be read as {want[1]!r}")
        return
    if want[0] == VALUE:
        # `0` and `0.0` compare equal, and the loader keeps them apart: a decimal
        # is an integer and a float is not, and that is what a schema's `maximum`
        # is answered from. So the Python type is part of the answer.
        if type(answer) is not type(want[1]) or answer != want[1]:
            failures.append(
                f"{what!r}: read as {answer!r} ({type(answer).__name__}), "
                f"must be {want[1]!r} ({type(want[1]).__name__})"
            )
        return
    if answer is not None:
        failures.append(f"{what!r}: a string came back with an answer of {answer!r}")


def read(text, value):
    check(text, None, (VALUE, value))


def string(text):
    check(text, None, (TEXT,))


def refused(text, problem, fix):
    check(text, None, (REFUSED, problem, fix))


def quote(text):
    """The fix every refused spelling ends with, in the loader's own words."""
    return f'quote it ("{text}")'


def number_fix(text, number):
    return f"write {number} for the number, or {quote(text)} for the string"


def one_version(text, number):
    """The refusal of a number only YAML 1.1 reads, naming the number it reads."""
    return (
        f'unquoted "{text}" is a number in YAML 1.1 and a string in YAML 1.2 and JSON',
        number_fix(text, number) if number is not None else f"{quote(text)} for the string",
    )


def no_json(text):
    return (f"{text} cannot be written in JSON", f"{quote(text)} for the string")


def leading_zero(text, read_as, fix):
    return (
        f'unquoted "{text}" has a leading zero, which YAML 1.1 reads as {read_as} '
        f"and YAML 1.2 as decimal",
        fix,
    )


def core_scalars():
    """What the core schema reads, as the loader's `core_schema_scalars` reads it."""
    read("true", True)
    read("True", True)
    read("TRUE", True)
    read("false", False)
    read("False", False)
    read("FALSE", False)
    for word in ("~", "null", "Null", "NULL", ""):
        read(word, None)
    read("12", 12)
    read("-3", -3)
    read("+3", 3)
    read("0", 0)
    read("-0", 0)
    read("+0", 0)
    # The core schema's octal and hexadecimal, which YAML 1.1 has no marker for
    # and PyYAML therefore reads as strings.
    read("0x1F", 31)
    read("0o17", 15)
    read("1.5", 1.5)
    # An exponent needs no dot: JSON and the core schema read these as numbers,
    # where PyYAML reads them as strings.
    read("1e3", 1000.0)
    read("1E3", 1000.0)
    read("1e+3", 1000.0)
    read("1.5e3", 1500.0)
    read(".5", 0.5)
    read("-.5", -0.5)
    read("5.", 5.0)
    read("1.e3", 1000.0)
    # A float's leading zero is no integer's leading zero.
    read("00.5", 0.5)
    read("01.5", 1.5)
    read("-01.5", -1.5)
    read("0e0", 0.0)
    # Digits past 64 bits are still the number the loader keeps its digits of,
    # and Python holds them exactly the same way.
    read("18446744073709551615", 18446744073709551615)
    read("18446744073709551616", 18446744073709551616)
    read("123456789012345678901234567890", 123456789012345678901234567890)
    read("-98765432109876543210", -98765432109876543210)
    # Text neither reading makes a number of.
    for text in ("1.2.3", "review", "08:30:00", "2026-11-21", "1.5.1", "a1", "1a", "-", "+",
                 "0x", "0b", "0o", "0x-", "0x-ZZZ", "0xZZ", "-0xZZ", "-0x", "-0b2", "-0o8",
                 "0o8", "0B11", "0b2", "_1000", "_1:30", "x_1", "1_x", "08_9", "01:30",
                 "1:70", "1:77", "0:00", "12:30:99", "1:0:70", "12:3a", "1:2_0", "1:2:30_0",
                 # An underscore in a float is no integer spelling, so no rule
                 # here reads it: the core schema reads this text as a string.
                 "1_000.5", "-1_000.5",
                 # `0o` is YAML 1.2's marker and YAML 1.1 has no such thing, so an
                 # underscore in it is a string to both.
                 # Two dots and no digits at all are no float's shape.
                 "0o7_7", "0x1.5", "00:00", "+.5e", "1e", "1.5e", "e5", "5..5", ".", ".."):
        string(text)


def booleans_only_yaml_11_reads():
    """`yes`, `no`, `on` and `off`, in any letter case, which 1.1 reads as bools."""
    for word in ("yes", "Yes", "YES", "no", "No", "NO", "on", "On", "ON", "off", "Off", "OFF"):
        refused(
            word,
            f'unquoted "{word}" is a boolean in YAML 1.1 and a string in YAML 1.2',
            f"write true or false for a boolean, or {quote(word)} for the string",
        )


def leading_zero_integers():
    """A leading zero, which is a decimal in 1.2 and an octal -- or nothing --
    in 1.1, refused whatever it lands on."""
    for text, read_as, fix in (
        ("0200", "128", "write 200, or 0o200 for octal, or quote it (\"0200\")"),
        ("010", "8", "write 10, or 0o10 for octal, or quote it (\"010\")"),
        ("007", "7", "write 7, or 0o7 for octal, or quote it (\"007\")"),
        ("-0200", "-128", "write -200, or -0o200 for octal, or quote it (\"-0200\")"),
        ("+0200", "128", "write 200, or 0o200 for octal, or quote it (\"+0200\")"),
        ("00", "0", "write 0, or 0o0 for octal, or quote it (\"00\")"),
        ("-00", "0", "write -0, or -0o0 for octal, or quote it (\"-00\")"),
        ("+00", "0", "write 0, or 0o0 for octal, or quote it (\"+00\")"),
        # An 8 or a 9 is no octal digit, so YAML 1.1 reads the text as a string
        # rather than as an integer of either base, and no octal fix exists.
        ("08", "a string", "write 8, or quote it (\"08\")"),
        ("06008", "a string", "write 6008, or quote it (\"06008\")"),
        ("09", "a string", "write 9, or quote it (\"09\")"),
        ("0089", "a string", "write 89, or quote it (\"0089\")"),
    ):
        refused(text, *leading_zero(text, read_as, fix))
    # Octal digits, but too many for any number to name, so the message names no
    # octal and offers no octal fix.
    too_many = "0" + "7" * 40
    refused(
        too_many,
        *leading_zero(
            too_many,
            "an octal number",
            f"write {'7' * 40}, or quote it (\"{too_many}\")",
        ),
    )


def signs_around_a_base_marker():
    """A sign on either side of `0x` or `0o`: a number in 1.1 on one side, no
    number in either version on the other, refused both ways."""
    for text, name, number in (
        ("-0x1F", "hexadecimal", "-31"),
        ("+0x1F", "hexadecimal", "31"),
        ("-0o17", "octal", "-15"),
        ("+0o17", "octal", "15"),
        # One digit is still a prefixed number, and the sign still sits on the
        # wrong side of it.
        ("-0x1", "hexadecimal", "-1"),
    ):
        refused(
            text,
            f'unquoted "{text}" puts a sign before its {name} prefix, which YAML 1.1 reads as '
            f"{number} and YAML 1.2 as a string",
            number_fix(text, number),
        )
    for text, marker, number in (
        ("0x-1F", "0x", "-31"),
        ("0x+1F", "0x", "31"),
        ("0x-1", "0x", "-1"),
        ("0o-17", "0o", "-15"),
    ):
        refused(
            text,
            f'unquoted "{text}" puts a sign after its {marker}, which is no number in either '
            f"YAML version",
            number_fix(text, number),
        )
    # Digits past what a signed 64-bit integer holds, in a base the loader has no
    # number for: refused, and told to write the decimal.
    for text, name in (
        ("0xFFFFFFFFFFFFFFFF", "hexadecimal"),
        ("0x1" + "0" * 16, "hexadecimal"),
        ("0o1" + "0" * 21, "octal"),
    ):
        refused(
            text,
            f"{text} is a {name} integer past signed 64 bits, which this loader does not read "
            f"as a number",
            f"write it in decimal, or {quote(text)}",
        )
    # Within signed 64 bits the same bases are read, not refused.
    read("0x7FFFFFFFFFFFFFFF", 2**63 - 1)
    read("0o777", 511)


def numbers_only_yaml_11_reads():
    """Binary, an underscore in a whole number and the sexagesimal: numbers to
    YAML 1.1, strings to the core schema and JSON."""
    for spelling, number in (
        ("0b11", "3"),
        ("-0b11", "-3"),
        ("0b1_1", "3"),
        ("0b_", None),
        ("1_000", "1000"),
        ("-1_0", "-10"),
        ("1000_", "1000"),
        ("1__000", "1000"),
        ("0x_1F", "31"),
        ("0x1_", "1"),
        ("0x_", None),
        ("0x__", None),
        ("07_7", "63"),
        ("010_7", "71"),
        ("0_", "0"),
        ("12:30", "750"),
        ("1:30:00", "5400"),
        ("1:2:3:4:5", "13403045"),
        ("-12:30", "-750"),
        ("1_2:30", "750"),
        ("1:07", "67"),
    ):
        refused(spelling, *one_version(spelling, number))


def what_json_cannot_hold():
    """Infinities and not-a-numbers, spelled either way, and an exponent no float
    can hold, which reads as the same infinity."""
    for spelling in (
        ".inf",
        ".Inf",
        ".INF",
        "+.inf",
        "+.Inf",
        "+.INF",
        "-.inf",
        "-.Inf",
        "-.INF",
        ".nan",
        ".NaN",
        ".NAN",
    ):
        refused(spelling, *no_json(spelling))
    for spelling in ("1e999", "-1e999", "+1e999", "1e400", ".5e999"):
        refused(spelling, *no_json(spelling))


def rule(what, got, want, why):
    """One answer from a rule this port holds for the checker's key path."""
    global cases
    cases += 1
    if got != want:
        failures.append(f"{what!r}: {why}")


def the_key_rule():
    """The two rules the checker's key path shares with this port, so that a
    shared edit to a shape here cannot quietly change a key's answer."""
    for spelling in (
        "0",
        "-5",
        "9223372036854775807",
        "9223372036854775808",
        "18446744073709551615",
    ):
        rule(
            spelling,
            core.canonical_integer_key(spelling),
            True,
            "must be read as a key that is its own text",
        )
    for spelling in (
        "-0",
        "+2",
        "00",
        "0200",
        "08",
        "0x1F",
        "0o17",
        "1e3",
        "1.5",
        "18446744073709551616",
        "99999999999999999999",
        "yes",
        "true",
        "",
    ):
        rule(
            spelling,
            core.canonical_integer_key(spelling),
            False,
            "is not a key that is its own text",
        )
    # Only YAML 1.1 reads these, and the key rule refuses them for that reason.
    for spelling in ("0b11", "1_000", "12:30", "0x_1F", "07_7", "1:30"):
        rule(
            spelling,
            core.one_version_integer(spelling),
            True,
            "is a number YAML 1.1 alone reads, and the key rule refuses it for that",
        )
    # And these are not, because the core schema reads them itself or nothing
    # reads a number out of them.
    for spelling in ("200", "0x1F", "0o17", "0200", "-0x1F", "0x-1F", "1e3", "08", "a_b", "_1"):
        rule(
            spelling,
            core.one_version_integer(spelling),
            False,
            "is no number YAML 1.1 alone reads",
        )


def main() -> int:
    core_scalars()
    booleans_only_yaml_11_reads()
    leading_zero_integers()
    signs_around_a_base_marker()
    numbers_only_yaml_11_reads()
    what_json_cannot_hold()
    the_key_rule()
    if failures:
        # Each failed case on a line of its own, starting with FAIL, which is how
        # `crates/atomic/tests/catalog_checker.rs` sees a case that did not pass
        # even before it looks at the exit status.
        print(f"{len(failures)} of {cases} cases failed:", file=sys.stderr)
        for one in failures:
            print(f"FAIL {one}", file=sys.stderr)
        return 1
    print(
        f"PASS scalar mirror: {cases} plain scalars read, refused or left as text "
        f"as atomic's loader reads them"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
