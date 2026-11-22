#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Every control is on in this tree.

scripts/controls-check.sh switches a control off in the source, runs the test
that guards it, and puts the source back. A commit made while a run was editing,
or a run that ended before it put the files back, can leave a control switched
off in what gets committed. Nothing else notices: a row's own test passes
against a disabled control whenever that test is skipped, and
scripts/controls-check.sh costs a rebuild per control and so does not run in CI.

So for each row under scripts/controls/, this reads the file the row names and
requires that

  * the row's switch text appears there exactly once, and
  * where the row's replacement holds that switch text, the replacement does not
    appear there at all.

Together those say the control is where the row says it is, and is the control
rather than what switching it off writes. Which of the two says it depends on the
row: switching off takes the switch text out of the file whenever the
replacement does not hold it, so for such a row the count is what reports a
control committed off, and its replacement is not asked of the file at all --
asking it would refuse a row whose replacement is a word the file has good cause
to say elsewhere, which is a row that cannot be fixed without moving the control,
not a tree with a control off. Where a replacement does hold its own switch text,
switching off leaves that text in place and the count cannot see the difference,
so there the replacement is what is asked: the file holding what switching off
writes is the control off. What is asked of the file there is the replacement as
written and with all its whitespace taken out, because several statements a row
puts on one line are not left there by rustfmt, which gives each a line of its
own: the exact text is then gone from a tree that still holds the switch text
exactly once, and only what is left when the layout is out of both says whether
the statements are in the file. A row whose replacement is empty deletes its switch
text and writes nothing to look for, so only the count applies to it; that rule
alone says the switch is where it points. Where a row names a file that cannot be
read, neither rule can be asked and the row is refused as what could not be read,
never as a control that is simply absent.

Every row file is read the way scripts/controls-check.sh reads it, since a row
read wrongly is a control that goes unchecked while the tree is reported as
holding all of them on. The two refuse the same rows: a file holding anything but
its one row -- one trailing newline being how a file ends rather than a second
row, so a file written without one is malformed to neither -- a row whose fields
are not five, a row that names no control, a name another row file already names,
and a name that is not the file's own. Past that python refuses three rows the
shell goes on to run, because the shell has no way to ask them: a row file that is
not text, a row whose switch text is empty, and a switch text its file holds more
than once. Each is a row that switches nothing the row means, so refusing it fails
closed where running it would say nothing. Anything that cannot be read, cannot be
found, or cannot be decoded is a refusal, and only a file that is genuinely not
there is reported as missing rather than unreadable. Every row is checked before
the run ends, so a commit that switched two controls off names both.

A row's fields are passed on with no interpretation at all, which is why this is
python rather than a shell loop: a row's text holds both `"` and `'`, and a
shell would have to unescape what it hands to a counter.

  python3 scripts/controls-on.py    # exits 0 when every control is on, else 1
"""

import os
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ROWS = ROOT / "scripts" / "controls"


def read_text(path):
    """The file's text, exactly as its bytes, or an error to refuse on.

    No newline translation: what is counted is what the file holds, which is
    what a row's text has to match byte for byte when it is replaced.
    """
    with open(path, encoding="utf-8", newline="") as handle:
        return handle.read()


def row_entries():
    """The entries under scripts/controls/, in file-name order.

    A directory that is not there, that holds nothing, or that cannot be listed
    is a tree with no controls to check, which is how every control would be
    lost at once and the run still report ok, so each of those is fatal. A
    directory that cannot be read is named as what could not be read and one
    that is not there at all as not there, which is what makes the first a
    different refusal from the second.
    """
    try:
        entries = list(os.scandir(ROWS))
    except FileNotFoundError:
        return None, f"{ROWS} is not there, so there is no control to check"
    except NotADirectoryError:
        return None, f"{ROWS} is not a directory, so there is no control to check"
    except OSError as error:
        # Permission denied, a link loop, too many open files: nothing can be
        # said about the controls, so nothing may be reported as on.
        return None, f"cannot list {ROWS}: {error.strerror}"
    if not entries:
        return None, f"{ROWS} holds no control"
    entries.sort(key=lambda entry: os.fsencode(entry.name))
    return entries, None


def main():
    entries, fatal = row_entries()
    if fatal:
        print(f"FAIL: {fatal}", file=sys.stderr)
        return 1

    problems = []
    checked = 0
    by_name = {}
    for entry in entries:
        row_file = f"scripts/controls/{entry.name}"
        kind = entry.name
        try:
            # A symlink is followed, as the shell's test for a regular file
            # follows it; a broken one, a directory, a FIFO and a link loop are
            # not regular files and are refused, not passed over.
            if not entry.is_file():
                problems.append(f"{kind}: {row_file} is not a regular file")
                continue
            if not entry.name.endswith(".row"):
                problems.append(
                    f"{kind}: {row_file} is not named <name>.row, so its control "
                    "would never be read"
                )
                continue
            text = read_text(entry.path)
        except OSError as error:
            problems.append(f"{kind}: {row_file} cannot be read: {error.strerror}")
            continue
        except UnicodeDecodeError:
            problems.append(f"{kind}: {row_file} is not text")
            continue

        # One row per file: five fields, so four separators. At most one
        # trailing newline comes off first: that is how the file ends rather than
        # a second row, which is what scripts/controls-check.sh's mapfile sees,
        # so a row file written without the newline is refused by neither and one
        # holding a blank line after its row is refused by both. A file holding
        # nothing at all holds no row, which is why empty text counts as zero
        # lines and not as one blank one. A file that does not say which control
        # it is names nothing to check, so it is refused by name before anything
        # else is asked of it.
        if text.endswith("\n"):
            text = text[:-1]
        lines = text.split("\n") if text else []
        if len(lines) != 1:
            problems.append(
                f"{kind}: {row_file} holds {len(lines)} rows, not the one it must"
            )
            continue
        fields = lines[0].split("|")
        if len(fields) != 5:
            problems.append(
                f"{kind}: {row_file} has {len(fields) - 1} '|' separators, not the 4 "
                "between its five fields; a field may not hold a '|'"
            )
            continue
        name, rel, switch, replacement, _args = fields
        if not name:
            problems.append(f"{kind}: {row_file} names no control")
            continue
        # Before the row's own name is asked of the file: two row files naming
        # one control are refused for the name, which is what makes the second
        # one wrong, and it is refused before its fields are read at all.
        if name in by_name:
            problems.append(
                f"{kind}: {row_file} names the control '{name}', which another row "
                "file already names"
            )
            continue
        by_name[name] = row_file
        if name != entry.name[: -len(".row")]:
            problems.append(
                f"{kind}: {row_file} names itself '{name}', which is not its file name"
            )
            continue
        kind = name

        # Where the control lives. A file that is not there and a file that
        # cannot be read are different refusals: the second is never treated as
        # the first, or an unreadable source would read as a control merely
        # absent, and a check that only looked for its switch as saying nothing.
        target = ROOT / rel
        try:
            source = read_text(target)
        except FileNotFoundError:
            problems.append(
                f"{kind}: {row_file} names {rel}, which is not there; update "
                f"{row_file} with the code"
            )
            continue
        except NotADirectoryError:
            problems.append(
                f"{kind}: {row_file} names {rel}, which is not a file and so holds "
                "no control"
            )
            continue
        except IsADirectoryError:
            problems.append(
                f"{kind}: {row_file} names {rel}, which is a directory and so holds "
                "no control"
            )
            continue
        except OSError as error:
            problems.append(
                f"{kind}: {row_file} names {rel}, which cannot be read: {error.strerror}"
            )
            continue
        except UnicodeDecodeError:
            problems.append(f"{kind}: {row_file} names {rel}, which is not text")
            continue

        # The control is on: what switching it off writes is not in the file at
        # all, and its switch is there once.
        if not switch:
            problems.append(
                f"{kind}: {row_file} switches nothing, so nothing in {rel} is its "
                "control"
            )
            continue
        # A row whose replacement holds its own switch text leaves that text in
        # the file when it switches the control off, so the count below cannot
        # see the difference and this is what can. For every other row switching
        # off takes the switch text out, the count reports it, and asking this
        # rule as well would refuse a row over a replacement that is a word the
        # file has good cause to say elsewhere.
        #
        # Asked with the whitespace out of both as well as as written, because a
        # replacement holding several statements on one line is not left there by
        # rustfmt, which gives each a line of its own. The exact text is then gone
        # from a tree that still holds the switch text exactly once, and what is
        # left when the layout is out of both is the only thing that says the
        # statements are in the file.
        if switch in replacement and (
            replacement in source
            or "".join(replacement.split()) in "".join(source.split())
        ):
            problems.append(
                f"{kind}: what switching it off writes is in {rel}, so this tree holds "
                "the control switched off. Put the control back, or give the row a "
                "replacement that does not hold its own switch text"
            )
            continue
        found = source.count(switch)
        if found == 0:
            problems.append(
                f"{kind}: its switch text is not in {rel}, so the control is not the "
                f"one this row switches off. Update {row_file} with the code"
            )
            continue
        if found > 1:
            problems.append(
                f"{kind}: its switch text appears {found} times in {rel}, not once, so "
                f"which site this row switches is not fixed. Update {row_file} with "
                "the code"
            )
            continue
        checked += 1

    if problems:
        for problem in problems:
            print(f"FAIL {problem}", file=sys.stderr)
        # One line per row file refused, which is what a reviewer fixes: the
        # count never says fewer than the rows involved, and a row refused twice
        # over is named twice.
        print(
            f"scripts/controls-on.py: {len(problems)} row(s) name a control that is "
            "not on in this tree",
            file=sys.stderr,
        )
        return 1
    print(f"ok: {checked} controls are on in this tree")
    return 0


if __name__ == "__main__":
    sys.exit(main())
