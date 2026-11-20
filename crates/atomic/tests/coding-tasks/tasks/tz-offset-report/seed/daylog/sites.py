"""The sites file: each site's UTC offset and display name."""

import json
from dataclasses import dataclass
from datetime import timedelta

from .offsets import parse_offset


class SiteError(Exception):
    """The sites file is unreadable or wrong, or names no such site."""


@dataclass(frozen=True)
class Site:
    key: str
    name: str
    offset_text: str
    offset: timedelta


def load_sites(path):
    """Every site in the file, by key."""
    try:
        with open(path, encoding="utf-8") as f:
            raw = json.load(f)
    except FileNotFoundError:
        raise SiteError(f"no such sites file: {path}") from None
    except json.JSONDecodeError as e:
        raise SiteError(f"{path}: not JSON: {e.msg}") from None
    if not isinstance(raw, dict):
        raise SiteError(f"{path}: expected an object of sites")
    sites = {}
    for key, entry in raw.items():
        if not isinstance(entry, dict) or "offset" not in entry:
            raise SiteError(f"{path}: site {key} has no offset")
        try:
            offset = parse_offset(entry["offset"])
        except ValueError as e:
            raise SiteError(f"{path}: site {key}: {e}") from None
        sites[key] = Site(key=key, name=entry.get("name", key), offset_text=entry["offset"], offset=offset)
    return sites


def adhoc_site(offset_text):
    """A site for --offset, named after its offset."""
    return Site(key="adhoc", name=f"Offset {offset_text}", offset_text=offset_text, offset=parse_offset(offset_text))
