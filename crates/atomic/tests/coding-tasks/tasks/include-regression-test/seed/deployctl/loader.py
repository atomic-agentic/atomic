"""Loading a deploy file and everything it includes into one Config."""

import os
from dataclasses import dataclass, field

from deployctl.parser import ParseError, parse_text

MAX_DEPTH = 8


class LoadError(Exception):
    def __init__(self, path, line, message):
        super().__init__(f"{path}:{line}: {message}")
        self.path = path
        self.line = line
        self.message = message


@dataclass(frozen=True)
class Value:
    text: str
    path: str
    line: int


@dataclass
class Config:
    root: str  # the directory of the file that was loaded
    sections: dict = field(default_factory=dict)  # section -> key -> Value
    files: list = field(default_factory=list)  # every file read, in order

    def get(self, section, key, default=None):
        value = self.sections.get(section, {}).get(key)
        return default if value is None else value.text

    def value(self, section, key):
        return self.sections.get(section, {}).get(key)


def resolve_include(target):
    """The absolute path an include line points at, with ~ expanded."""
    return os.path.abspath(os.path.expanduser(target))


def read_file(path):
    with open(path, encoding="utf-8") as f:
        return f.read()


def load(path):
    """The Config for the deploy file at path, with its includes merged in where they appear.

    Later settings replace earlier ones. Raises LoadError for a file that cannot be read or parsed, an include
    that is missing, a cycle of includes, or includes nested more than MAX_DEPTH deep.
    """
    top = os.path.abspath(path)
    config = Config(root=os.path.dirname(top))
    try:
        text = read_file(top)
    except OSError as error:
        raise LoadError(top, 0, f"cannot read: {error.strerror}") from None
    _merge(config, top, text, stack=[top])
    return config


def _merge(config, path, text, stack):
    try:
        items = parse_text(text, path)
    except ParseError as error:
        raise LoadError(error.path, error.line, error.message) from None
    config.files.append(path)
    section = None
    for item in items:
        if item.kind == "section":
            section = config.sections.setdefault(item.name, {})
        elif item.kind == "set":
            section[item.name] = Value(item.value, path, item.line)
        else:
            target = resolve_include(item.value)
            if target in stack:
                raise LoadError(path, item.line, f"include cycle through {target}")
            if len(stack) >= MAX_DEPTH:
                raise LoadError(path, item.line, f"includes nested more than {MAX_DEPTH} deep")
            try:
                included = read_file(target)
            except FileNotFoundError:
                raise LoadError(path, item.line, f"include not found: {target}") from None
            except OSError as error:
                raise LoadError(path, item.line, f"cannot read {target}: {error.strerror}") from None
            _merge(config, target, included, stack + [target])
