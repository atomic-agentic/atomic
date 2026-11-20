"""Reading one deploy file into items: sections, settings and includes, each with its line number."""

import re
from dataclasses import dataclass

SECTION = re.compile(r"^\[([a-z0-9_.-]+)\]$")
SETTING = re.compile(r"^([a-z_][a-z0-9_]*)\s*=\s*(.*)$")


class ParseError(Exception):
    def __init__(self, path, line, message):
        super().__init__(f"{path}:{line}: {message}")
        self.path = path
        self.line = line
        self.message = message


@dataclass(frozen=True)
class Item:
    kind: str  # "section", "set" or "include"
    line: int
    name: str = ""
    value: str = ""


def unquote(text):
    """A value without its surrounding double quotes, if it has them. \\" and \\\\ work inside them."""
    if len(text) >= 2 and text[0] == '"' and text[-1] == '"':
        inner = text[1:-1]
        return inner.replace('\\"', '"').replace("\\\\", "\\")
    return text


def parse_text(text, path):
    """The items in text, in order. path is only used in errors."""
    items = []
    in_section = False
    for number, raw in enumerate(text.splitlines(), start=1):
        line = raw.strip()
        if not line or line.startswith(("#", ";")):
            continue
        match = SECTION.match(line)
        if match:
            items.append(Item("section", number, match.group(1)))
            in_section = True
            continue
        if line.startswith("["):
            raise ParseError(path, number, f"bad section header: {line}")
        match = SETTING.match(line)
        if not match:
            raise ParseError(path, number, f"not a setting: {line}")
        key, value = match.group(1), unquote(match.group(2).strip())
        if key == "include":
            if not value:
                raise ParseError(path, number, "include needs a file")
            items.append(Item("include", number, value=value))
        elif not in_section:
            raise ParseError(path, number, f"{key} is outside a section")
        else:
            items.append(Item("set", number, key, value))
    return items
