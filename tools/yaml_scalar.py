#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""What atomic's YAML loader makes of a plain scalar, in text, with no third-party module.

`crates/atomic-schema/src/yaml.rs` classifies a plain (unquoted) scalar by its own
text, because the same text is read different ways by YAML 1.1, YAML 1.2 and JSON:
`0o17` is the number 15 to the core schema and a string to YAML 1.1, `12:30` is 750
to YAML 1.1 and a string to the core schema, `1e3` is 1000.0 to JSON and a string to
PyYAML. So nothing here can depend on what a YAML library constructs; the text is
asked, and `core_scalar` answers one of three things about it:

  * TEXT    -- neither a null, a bool nor a number: the text, read as a string;
  * VALUE   -- a null, a bool or a number that all three read alike;
  * REFUSED -- a spelling the loader will not take, with what the versions read
               differently and what to write instead.

`tools/check_examples.py` reads every schema file through this, so an example whose
sidecar spells a value the way atomic's loader refuses is refused here too, and one
spelled `0o17` or `1e3` is read as the number the loader reads rather than the string
PyYAML resolves. The rules are read in the order the loader reads them: the null and
boolean words, then the words only YAML 1.1 reads as a boolean, then its integers,
then its floats, then the numbers only YAML 1.1 reads, then what JSON cannot hold.

`one_version_integer` and `canonical_integer_key` sit here beside the classifier
because they read the same shapes: the first is what `core_scalar` refuses as YAML
1.1's alone, and the second is what a plain key must be for the loader to take it as
its own text.

Only `str` and `re` are used, so this runs in the development image, where the YAML
library that reads the catalog is not installed. Where that library resolves a text
to something this module has no answer for -- an ISO date, which YAML 1.1 resolves
as a timestamp and the core schema reads as a string -- the checker refuses the text
rather than read it as this module's answer, because a checker that quietly re-types
what the library resolved has decided nothing.

    python3 tools/yaml_scalar_test.py
"""

from __future__ import annotations

import re

# What `core_scalar` says a plain scalar is. The loader says the same three things
# with `Core::Text`, `Core::Value` and `Core::Refused`.
TEXT = "text"
VALUE = "value"
REFUSED = "refused"

# Plain scalars the core schema reads as a number. CORE_INT is its integers --
# decimal, `0o` octal and `0x` hexadecimal -- and CORE_FLOAT its floats, which
# need no dot when they have an exponent and no digit before a dot.
CORE_INT = re.compile(r"^[-+]?(?:0|[1-9][0-9]*|0o[0-7]+|0x[0-9a-fA-F]+)$")
CORE_FLOAT = re.compile(r"^[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?$")
# A key the loader reads as its own text: `0` or `-?[1-9][0-9]*`.
CANONICAL_INT = re.compile(r"^(?:0|-?[1-9][0-9]*)$")
LEADING_ZERO_INT = re.compile(r"^[-+]?0[0-9]+$")
# A sign on the wrong side of the base marker: `-0x1F`, which YAML 1.1 reads as
# a number where 1.2 reads a string, and `0x-1F`, which no version reads as a
# number at all. Both are refused, and quoting is the fix either way.
SIGN_ON_A_BASE_MARKER = re.compile(r"^(?:[-+]0[ox][0-9a-fA-F]+|0[ox][-+][0-9a-fA-F]+)$")
INF_OR_NAN = re.compile(r"^(?:[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))$")
YAML_11_BOOLS = {"yes", "no", "on", "off"}
# YAML 1.1's integer rules exactly as PyYAML resolves them, in the order its
# resolver tries them. An underscore counts anywhere after the first digit, so
# `1000_` and `0x_1F` are integers to it, and a sexagesimal starts at 1 and
# counts its groups in base 60. `0o` is absent because YAML 1.1 writes an octal
# `0777` or `o777`, never `0o777`: PyYAML reads `0o7_7` as a string, and the
# loader refuses that spelling only because 1.1's own type definition has an
# `o` marker an underscore may sit in.
YAML_11_BINARY = re.compile(r"0b[01_]+$")
YAML_11_SEXAGESIMAL = re.compile(r"[1-9][0-9_]*(?::[0-5]?[0-9])+$")
YAML_11_OTHER = re.compile(r"(?:0x[0-9a-fA-F_]+|0[0-7_]+|0|[1-9][0-9_]*)$")

# The words the core schema reads as null and as the two booleans, exactly as the
# loader matches them.
NULLS = ("", "~", "null", "Null", "NULL")
TRUES = ("true", "True", "TRUE")
FALSES = ("false", "False", "FALSE")
# The loader matches these exact spellings after asking whether only YAML 1.1 has
# read a number out of the text, and refuses them because JSON has no form for
# what the core schema reads.
INF_NAN = (
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
)

# What a signed 64-bit integer holds, and a signed 128-bit one. A decimal past
# 64 bits is still an integer to JSON and the loader keeps its digits, which
# Python's integers hold exactly too; a base-8 or base-16 integer past it the
# loader refuses outright.
I64_MIN = -(2**63)
I64_MAX = 2**63 - 1
U64_MAX = 2**64 - 1
I128_MIN = -(2**127)
I128_MAX = 2**127 - 1
RADIX_DIGITS = "0123456789abcdefghijklmnopqrstuvwxyz"
DECIMALS = "0123456789"


def is_digit(c, radix):
    """Whether `c` is a digit this radix has, as Rust's `char::is_digit` answers
    it. Only the radices this file reads -- 8, 10 and 16 -- are ever asked."""
    return c.lower() in RADIX_DIGITS[:radix]


class Refusal:
    """Why a plain scalar is refused: what the YAML versions read differently, and
    what to write instead. A value gets both, because the author meant one of the
    two; a key is fixed only by quoting, so the key rule names the key, quotes the
    reason and drops the advice. The loader's `Refusal` says the same two things."""

    def __init__(self, problem, fix):
        self.problem = problem
        self.fix = fix

    def as_value(self):
        return f"{self.problem}; {self.fix}"

    def as_key(self):
        return f"{self.problem}; quote it"


def quoted(text):
    """How the loader writes a refused spelling in a message: `"0200"`."""
    return f'"{text}"'


def split_sign(text):
    """The sign a plain number spelling starts with, and the rest of it."""
    if text.startswith("-"):
        return "-", text[1:]
    if text.startswith("+"):
        return "+", text[1:]
    return None, text


def sign_text(sign):
    """The sign as text. A plus is no part of the number to write back, so only a
    minus is written."""
    return "-" if sign == "-" else ""


def every(text, digit):
    """Whether `text` is not empty and every character of it satisfies `digit`."""
    return text != "" and all(digit(c) for c in text)


def all_digits(digits, radix):
    """Whether `digits` is not empty and every character of it is a digit this
    radix has. An underscore is no digit, so this says no to it."""
    return digits != "" and all(is_digit(c, radix) for c in digits)


def base_of(body):
    """The base these digits are written in, as its radix, what to call it in a
    sentence, the marker an author writes before it, and the digits after that
    marker. Decimal is what is left when there is no marker."""
    if body.startswith("0o"):
        return 8, "octal", "0o", body[2:]
    if body.startswith("0x"):
        return 16, "hexadecimal", "0x", body[2:]
    return 10, "decimal", "", body


def number_text(sign, digits, radix):
    """The number these digits say, with the sign in front, or nothing when no
    number can hold it or when nothing but underscores is left. Underscores are
    no part of the number, and a decimal's leading zeros are only how the digits
    were written."""
    bare = digits.replace("_", "")
    if radix == 10:
        bare = bare.lstrip("0")
    if bare == "":
        return None
    try:
        n = int(bare, radix)
    except ValueError:  # nothing here is a digit of this base
        return None
    if sign == "-":
        n = -n
    if not I64_MIN <= n <= I64_MAX:
        return None
    return str(n)


class CoreInt:
    """What the core schema reads a plain scalar written like an integer as. The
    loader's `CoreInt`: `NOT_AN_INT`, a `NUMBER`, or a `REFUSAL`."""

    NOT_AN_INT = "not an int"
    NUMBER = "number"
    REFUSAL = "refusal"

    __slots__ = ("kind", "number", "refusal")

    def __init__(self, kind, number=None, refusal=None):
        self.kind = kind
        self.number = number
        self.refusal = refusal


def core_int(text):
    """Core schema integers: decimal with an optional sign, `0o` octal, `0x` hex.

    A leading zero and a sign written on either side of the base prefix are
    refused rather than read, because YAML 1.1 and YAML 1.2 do not read them
    alike, and a base-8 or base-16 integer past signed 64 bits is refused because
    this loader cannot read it as a number.
    """
    sign, body = split_sign(text)
    if every(body, lambda c: c in DECIMALS):
        if len(body) > 1 and body.startswith("0"):
            # YAML 1.2 reads the digits as decimal; YAML 1.1 reads them as an
            # octal, or as a string when a digit is no octal digit. The two do
            # not always land on a different number (`007` is 7 both ways), but
            # the spelling is a decimal in one version and an octal in the
            # other, so it is refused whatever it lands on.
            octal = number_text(sign, body, 8) if all_digits(body, 8) else None
            if octal is not None:
                read_as = octal
            elif all_digits(body, 8):
                # Octal digits, but too many for any number to name.
                read_as = "an octal number"
            else:
                # An 8 or a 9 is no octal digit, so YAML 1.1 reads the text as
                # a string rather than as an integer of either base.
                read_as = "a string"
            # What the digits say without the leading zeros, which is the
            # decimal to write; all zeros say zero.
            bare = body.lstrip("0") or "0"
            sign = sign_text(sign)
            octal_fix = f", or {sign}0o{bare} for octal" if octal is not None else ""
            return CoreInt(
                CoreInt.REFUSAL,
                refusal=Refusal(
                    f"unquoted {quoted(text)} has a leading zero, which YAML 1.1 reads as "
                    f"{read_as} and YAML 1.2 as decimal",
                    f"write {sign}{bare}{octal_fix}, or quote it ({quoted(text)})",
                ),
            )
        # Any decimal is a number, however many digits, as JSON reads it. Python
        # holds the digits exactly, which is what PyYAML has always done here; past
        # signed and unsigned 64 bits the loader has no number that exact and reads
        # a float, which is the one place its answer and this one differ by type.
        # Such a decimal is no key either way, because a key must be a number whose
        # digits are its own text -- see `canonical_integer_key`. And `every` said
        # the body is not empty and is all decimal digits, so this parse cannot
        # fail: a bare `+` or `-` never reaches here.
        return CoreInt(CoreInt.NUMBER, number=int(text))
    radix, name, marker, digits = base_of(body)
    if radix == 10 or digits == "":
        # Nothing here is a base-8 or base-16 spelling, or the marker has no
        # digits after it: either way this is a string, as the core schema reads
        # it. `every` says no to an empty body, so `-` and `+` on their own end
        # up here too.
        return CoreInt(CoreInt.NOT_AN_INT)
    # Which side of the marker the sign sits on decides the message: only a sign
    # before it is a number in YAML 1.1, and only a sign after it is a number in
    # neither.
    inner_sign, inner_digits = split_sign(digits)
    if inner_sign is not None and all_digits(inner_digits, radix):
        return CoreInt(
            CoreInt.REFUSAL,
            refusal=sign_around_prefix(text, inner_sign, False, radix, name, marker, inner_digits),
        )
    if sign is not None and all_digits(digits, radix):
        return CoreInt(
            CoreInt.REFUSAL,
            refusal=sign_around_prefix(text, sign, True, radix, name, marker, digits),
        )
    if not all_digits(digits, radix):
        # A digit the base has no such thing as, or an underscore, which is no
        # digit here though YAML 1.1 takes it: the core schema reads this as a
        # string, and only YAML 1.1 asks about it next.
        return CoreInt(CoreInt.NOT_AN_INT)
    n = int(digits, radix)  # every digit of this base, so this cannot fail
    if n > I64_MAX:
        return CoreInt(
            CoreInt.REFUSAL,
            refusal=Refusal(
                f"{text} is a {name} integer past signed 64 bits, which this loader does "
                f"not read as a number",
                f"write it in decimal, or quote it ({quoted(text)})",
            ),
        )
    return CoreInt(CoreInt.NUMBER, number=n)


def sign_around_prefix(text, sign, before, radix, name, marker, digits):
    """Why a sign written on the wrong side of a base marker is refused. `before`
    says which side: a sign before the marker is a number in YAML 1.1 and a
    string in the core schema, and a sign after it is no number in either.
    Either way the digits say the number the author meant."""
    number = number_text(sign, digits, radix)
    if before:
        problem = (
            f"unquoted {quoted(text)} puts a sign before its {name} prefix, which YAML 1.1 "
            f"reads as {number if number is not None else 'a number'} and YAML 1.2 as a string"
        )
    else:
        problem = (
            f"unquoted {quoted(text)} puts a sign after its {marker}, which is no number "
            f"in either YAML version"
        )
    if number is None:
        # No number can hold these digits, so only the string can be offered.
        fix = f"quote it ({quoted(text)}) for the string"
    else:
        fix = f"write {number} for the number, or quote it ({quoted(text)}) for the string"
    return Refusal(problem, fix)


class Yaml11:
    """A number that only YAML 1.1 reads, and the number it reads. The loader's
    `Yaml11`: binary (`0b11`), a whole number written with an underscore
    (`1_000`, `0x_1F`, `1000_`), or the sexagesimal (`12:30`)."""

    BINARY = "binary"
    UNDERSCORED = "underscored"
    SEXAGESIMAL = "sexagesimal"

    __slots__ = ("kind", "sign", "radix", "digits", "groups")

    def __init__(self, kind, sign, radix=10, digits="", groups=None):
        self.kind = kind
        self.sign = sign
        self.radix = radix
        self.digits = digits
        self.groups = groups

    def value(self):
        """What YAML 1.1 reads this as, named in the message so that an author who
        meant a number has it, or nothing when no number can hold the digits --
        `0b` with 129 of them, say. This text is refused, never published, so the
        read is only ever to name it."""
        if self.kind == Yaml11.SEXAGESIMAL:
            if self.groups is None:
                return None
            return f"{sign_text(self.sign)}{self.groups}"
        if self.kind == Yaml11.BINARY:
            bare = self.digits.replace("_", "")
            if bare == "" or not every(bare, lambda c: c in "01"):
                return None
            n = int(bare, 2)
            if n >= 2**128:  # the loader reads these through 128 bits
                return None
            return f"{sign_text(self.sign)}{n}"
        return number_text(self.sign, self.digits, self.radix)


def yaml_11_int(text):
    """What YAML 1.1's integer type reads this text as, if it reads it as a number
    at all. This follows the integer rules YAML 1.1 gives, which is what PyYAML
    resolves with and what `tools/check_examples.py` mirrors:

    ```text
    [-+]?0b[0-1_]+  |  [-+]?0[0-7_]+  |  [-+]?(0|[1-9][0-9_]*)
      |  [-+]?0x[0-9a-fA-F_]+  |  [-+]?[1-9][0-9_]*(:[0-5]?[0-9])+
    ```

    An underscore counts anywhere after the first digit, so `1000_` and `0x_1F`
    are numbers there and strings here, and only a leading underscore makes a
    string of both. A colon group is `[0-5]?[0-9]` and takes no underscore, and
    there is no `o` marker in this list: `0o17` is the core schema's octal, which
    `core_int` reads as 15. The shapes the core schema reads as numbers too --
    a plain decimal, a plain `0x` hex, a leading-zero octal -- are answered by
    `core_int` as well, which is asked first.
    """
    sign, body = split_sign(text)
    if "o" in body:
        # No class in that list holds an `o`, so nothing with one is a number
        # YAML 1.1 reads. `0o17` is YAML 1.2's octal, which `core_int` reads as
        # 15, and `0o7_7` is a string to both versions.
        return None
    if body.startswith("0b"):
        digits = body[2:]
        # `0b2` is no binary number and `0B11` is no binary marker, so both are
        # strings to every version. An underscore alone after the marker is in
        # the class YAML 1.1 gives its binary integers, which is why `0b_` is
        # refused even though no number reads out of it.
        if every(digits, lambda c: c in "_01"):
            return Yaml11(Yaml11.BINARY, sign, digits=digits)
        return None
    if body == "":
        return None
    if ":" in body:
        first, _, rest = body.partition(":")
        # The sexagesimal starts at 1 and counts every group after a colon in
        # base 60, so `01:30`, `1:70` and `12:3a` are no sexagesimal and stay
        # strings for both versions.
        # An underscore may sit anywhere after that first digit, never where
        # the digit is, so `_1:30` is no sexagesimal either.
        if first == "" or first[0] not in "123456789":
            return None
        head = first.replace("_", "")
        if not every(head, lambda c: c in DECIMALS):
            return None
        value = int(head)
        for group in rest.split(":"):
            # A colon group is `[0-5]?[0-9]`: two digits at most, and no
            # underscore, which the head allows and a group does not. So `1:70`
            # and `1:2_0` are no sexagesimal, as PyYAML reads them too, and a
            # group that does match is a base-60 digit by its shape.
            if len(group) == 1 and group in DECIMALS:
                digit = DECIMALS.index(group)
            elif len(group) == 2 and group[0] in "012345" and group[1] in DECIMALS:
                digit = DECIMALS.index(group[0]) * 10 + DECIMALS.index(group[1])
            else:
                return None
            # A chain of groups Python's unbounded integers hold and no number
            # here can name: still 1.1's number, still refused, only unnamed in
            # the message.
            if value is None:
                continue
            value = value * 60 + digit
            if not I128_MIN <= value <= I128_MAX:
                value = None
        return Yaml11(Yaml11.SEXAGESIMAL, sign, groups=value)
    if "_" not in body:
        return None
    _, _, marker, digits = base_of(body)
    first = digits[:1]
    if first == "":
        return None
    # Where the first digit is decides the base these digits are read in, as
    # the three whole-number alternatives say: a leading zero is an octal, `0_`
    # is zero that way too, and only a first digit from 1 up is a decimal. An 8
    # or a 9 after the zero is no octal digit, so no version reads a number.
    # Hex takes a hex digit in any place, and an underscore after the `0x`
    # marker sits in its class, as it does after `0b`.
    rest = digits[1:]
    if marker == "0x":
        read_as = 16
        head_ok = all(c == "_" or is_digit(c, 16) for c in digits)
    elif first == "0":
        read_as, head_ok = 8, True
    else:
        read_as, head_ok = 10, is_digit(first, 10)
    # An underscore may then sit anywhere after the first digit, but nothing
    # else may, and the first digit's own base was the question above.
    if head_ok and all(c == "_" or is_digit(c, read_as) for c in rest):
        return Yaml11(Yaml11.UNDERSCORED, sign, radix=read_as, digits=digits)
    return None


def core_float(text):
    """Core schema floats: `[-+]?(\\.[0-9]+|[0-9]+(\\.[0-9]*)?)([eE][-+]?[0-9]+)?`.
    An exponent needs no dot, which is why PyYAML reads `1e3` as a string where the
    core schema and JSON read 1000.0."""
    body = text[1:] if text[:1] in ("-", "+") else text
    at = len(body)
    for i, c in enumerate(body):
        if c in "eE":
            at = i
            break
    mantissa, exponent = body[:at], body[at + 1 :] if at < len(body) else None
    if "." in mantissa:
        integer, _, fraction = mantissa.partition(".")
        # Either side of the dot may be missing -- `.5` and `5.` are both floats
        # -- but not both, and every character there is a decimal digit. This is
        # `all`, not `every`: an empty side is allowed here and nowhere else.
        mantissa_ok = (integer != "" or fraction != "") and all(
            c in DECIMALS for c in integer
        ) and all(c in DECIMALS for c in fraction)
    else:
        mantissa_ok = every(mantissa, lambda c: c in DECIMALS)
    if exponent is None:
        exponent_ok = True
    else:
        digits = exponent[1:] if exponent[:1] in ("-", "+") else exponent
        exponent_ok = every(digits, lambda c: c in DECIMALS)
    if not (mantissa_ok and exponent_ok and ("." in mantissa or exponent is not None)):
        return None
    try:
        return float(text)
    except ValueError:
        # The shape is the grammar's but this Python will not read it, so the
        # loader's own parse fails the same way and the text stays a string. A
        # loader answers with a string or an error, never a raise.
        return None


def changed_across_yaml_versions(text):
    """Why a number only one YAML version reads is refused, naming the number it
    reads; nothing says so when both versions read the same thing."""
    one = yaml_11_int(text)
    if one is None:
        return None
    number = one.value()
    if number is None:
        # No number can hold these digits, so only the string can be offered.
        fix = f"quote it ({quoted(text)}) for the string"
    else:
        fix = f"write {number} for the number, or quote it ({quoted(text)}) for the string"
    return Refusal(
        f"unquoted {quoted(text)} is a number in YAML 1.1 and a string in YAML 1.2 and JSON",
        fix,
    )


def no_json(text):
    """The refusal of a scalar JSON has no form for, which quoting settles."""
    return Refusal(f"{text} cannot be written in JSON", f"quote it ({quoted(text)}) for the string")


def core_scalar(text):
    """What the YAML 1.2 core schema reads a plain (unquoted) scalar as, and what
    this loader does with the spellings it will not take.

    Returns `(TEXT, None)`, `(VALUE, the value)` or `(REFUSED, the Refusal)`,
    asking in the order the loader asks: the null and boolean words, the words
    only YAML 1.1 reads as a boolean, the core schema's integers, its floats, the
    integers only YAML 1.1 reads, and what JSON cannot hold.
    """
    if text in NULLS:
        return VALUE, None
    if text in TRUES:
        return VALUE, True
    if text in FALSES:
        return VALUE, False
    if text.lower() in YAML_11_BOOLS:
        return (
            REFUSED,
            Refusal(
                f"unquoted {quoted(text)} is a boolean in YAML 1.1 and a string in YAML 1.2",
                f"write true or false for a boolean, or quote it ({quoted(text)}) for the string",
            ),
        )
    whole = core_int(text)
    if whole.kind == CoreInt.NUMBER:
        return VALUE, whole.number
    if whole.kind == CoreInt.REFUSAL:
        return REFUSED, whole.refusal
    number = core_float(text)
    if number is not None:
        # JSON has no form for an infinity, whether it was spelled `.inf` or
        # came out of an exponent no float can hold.
        if number in (float("inf"), float("-inf")):
            return REFUSED, no_json(text)
        return VALUE, number
    # Asked only once neither reading has made a number of the text, so that
    # `1_000.5` stays the string the core schema reads and only a whole number
    # spelled with an underscore is refused.
    why = changed_across_yaml_versions(text)
    if why is not None:
        return REFUSED, why
    if text in INF_NAN:
        return REFUSED, no_json(text)
    return TEXT, None


def one_version_integer(spelling):
    """Whether YAML 1.1 alone reads this plain scalar as an integer.

    Binary (`0b11`), a whole number with an underscore in it (`1_000`,
    `0x_1F`, `07_7`) and the sexagesimal (`12:30`) are integers to YAML 1.1 and
    strings to the core schema and JSON. A spelling both read as an integer is
    left out: the loader has already read it as one or refused it for its own
    reason. `yaml_11_int` in the loader is the same rule.
    """
    if (
        CORE_INT.match(spelling)
        or LEADING_ZERO_INT.match(spelling)
        or SIGN_ON_A_BASE_MARKER.match(spelling)
    ):
        return False
    digits = re.sub(r"^[-+]", "", spelling)
    if YAML_11_BINARY.match(digits):
        return True
    if ":" in digits:
        return YAML_11_SEXAGESIMAL.match(digits) is not None
    return "_" in digits and YAML_11_OTHER.match(digits) is not None


def canonical_integer_key(spelling):
    """Whether the loader reads this plain scalar as a key that is its own text.

    That is `0` or `-?[1-9][0-9]*`, and only while a 64-bit integer can hold it:
    the loader reads any decimal as a number, however many digits, and keeps a
    key only while the number is one whose digits are the key, so a decimal past
    `18446744073709551615` is refused there; PyYAML, with Python's unbounded
    integers, reads either as the integer and would let the key through.
    """
    if not CANONICAL_INT.match(spelling):
        return False
    return I64_MIN <= int(spelling) <= U64_MAX
