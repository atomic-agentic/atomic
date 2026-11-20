"""Pricing settings for a profile: the defaults, then the profile's file, then the environment."""

import configparser
import os
import re
from dataclasses import dataclass
from decimal import Decimal, InvalidOperation

from .cache import memoize

DEFAULTS_FILE = "defaults.ini"
PROFILES_DIR = "profiles"
SECTION = "pricing"
ENV_PREFIX = "PRICER_"

_PROFILE_NAME = re.compile(r"[a-z][a-z0-9_]{0,15}")


class SettingsError(Exception):
    """The configuration is missing or wrong."""


@dataclass(frozen=True)
class Settings:
    profile: str
    label: str
    currency: str
    fx_rate: Decimal
    tax_rate: Decimal
    decimals: int
    bulk_threshold: int
    bulk_discount_pct: Decimal


def _decimal(text):
    try:
        value = Decimal(text)
    except InvalidOperation:
        raise ValueError(f"not a number: {text!r}") from None
    if not value.is_finite() or value < 0:
        raise ValueError(f"not a non-negative number: {text!r}")
    return value


def _count(text):
    if not text.strip().isdigit():
        raise ValueError(f"not a whole number: {text!r}")
    return int(text)


def _currency(text):
    text = text.strip().upper()
    if not re.fullmatch(r"[A-Z]{3}", text):
        raise ValueError(f"not a currency code: {text!r}")
    return text


# Each key and how its text becomes a value.
KEYS = {
    "label": str.strip,
    "currency": _currency,
    "fx_rate": _decimal,
    "tax_rate": _decimal,
    "decimals": _count,
    "bulk_threshold": _count,
    "bulk_discount_pct": _decimal,
}


def _read_section(path):
    """The [pricing] section of one file, as text."""
    parser = configparser.ConfigParser(interpolation=None)
    try:
        with open(path, encoding="utf-8") as f:
            parser.read_file(f, source=path)
    except FileNotFoundError:
        raise SettingsError(f"no such file: {path}") from None
    except configparser.Error as e:
        raise SettingsError(f"{path}: {e}") from None
    if not parser.has_section(SECTION):
        raise SettingsError(f"{path}: no [{SECTION}] section")
    values = dict(parser.items(SECTION))
    unknown = sorted(set(values) - set(KEYS))
    if unknown:
        raise SettingsError(f"{path}: unknown key {unknown[0]}")
    return values


def _env_overrides(profile):
    prefix = f"{ENV_PREFIX}{profile.upper()}_"
    found = {}
    for key in KEYS:
        text = os.environ.get(prefix + key.upper())
        if text is not None:
            found[key] = text
    return found


def profile_names(config_dir):
    """Every profile with a file, sorted."""
    directory = os.path.join(config_dir, PROFILES_DIR)
    try:
        names = os.listdir(directory)
    except FileNotFoundError:
        raise SettingsError(f"no profiles directory: {directory}") from None
    return sorted(n[: -len(".ini")] for n in names if n.endswith(".ini") and _PROFILE_NAME.fullmatch(n[:-4]))


@memoize
def load_settings(config_dir, *, profile):
    """The settings for one profile. Cached: see pricer.cache."""
    if not _PROFILE_NAME.fullmatch(profile):
        raise SettingsError(f"not a profile name: {profile!r}")
    text = _read_section(os.path.join(config_dir, DEFAULTS_FILE))
    profile_path = os.path.join(config_dir, PROFILES_DIR, profile + ".ini")
    if not os.path.exists(profile_path):
        raise SettingsError(f"no such profile: {profile}")
    text.update(_read_section(profile_path))
    text.update(_env_overrides(profile))
    missing = sorted(set(KEYS) - set(text))
    if missing:
        raise SettingsError(f"profile {profile}: no value for {missing[0]}")
    values = {}
    for key, convert in KEYS.items():
        try:
            values[key] = convert(text[key])
        except ValueError as e:
            raise SettingsError(f"profile {profile}, {key}: {e}") from None
    if values["decimals"] > 4:
        raise SettingsError(f"profile {profile}: decimals must be at most 4")
    if values["fx_rate"] == 0:
        raise SettingsError(f"profile {profile}: fx_rate must not be 0")
    return Settings(profile=profile, **values)
