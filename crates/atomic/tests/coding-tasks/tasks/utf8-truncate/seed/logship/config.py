"""Configuration: a small `key = value` file laid over the defaults."""

from dataclasses import dataclass

DEFAULTS = {
    "host": "localhost",
    "app": "logship",
    "facility": "1",
    "max_line_bytes": "120",
    "output": "out.log",
}

INTEGER_KEYS = ("facility", "max_line_bytes")


class ConfigError(ValueError):
    pass


@dataclass(frozen=True)
class Config:
    host: str = "localhost"
    app: str = "logship"
    facility: int = 1
    max_line_bytes: int = 120
    output: str = "out.log"


def parse(text):
    """Returns the settings in a configuration file's text, as strings."""
    settings = {}
    for number, raw in enumerate(text.splitlines(), start=1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if "=" not in line:
            raise ConfigError(f"line {number}: expected key = value")
        key, value = (part.strip() for part in line.split("=", 1))
        if key not in DEFAULTS:
            raise ConfigError(f"line {number}: unknown key {key!r}")
        settings[key] = value
    return settings


def build(settings):
    """A Config from string settings, with defaults for any that are missing."""
    merged = dict(DEFAULTS)
    merged.update(settings)
    values = {}
    for key, value in merged.items():
        if key in INTEGER_KEYS:
            try:
                values[key] = int(value)
            except ValueError:
                raise ConfigError(f"{key} must be an integer, not {value!r}") from None
        else:
            values[key] = value
    if not 0 <= values["facility"] <= 23:
        raise ConfigError("facility must be between 0 and 23")
    if values["max_line_bytes"] < 1:
        raise ConfigError("max_line_bytes must be positive")
    return Config(**values)


def load(path=None, overrides=None):
    """Reads a configuration file, if one is given, and applies overrides on top."""
    settings = {}
    if path is not None:
        with open(path, encoding="utf-8") as f:
            settings.update(parse(f.read()))
    for key, value in (overrides or {}).items():
        if value is not None:
            settings[key] = str(value)
    return build(settings)
