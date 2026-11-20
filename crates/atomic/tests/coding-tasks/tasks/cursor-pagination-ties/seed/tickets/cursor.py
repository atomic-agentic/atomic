"""Opaque page cursors: a position in the listing, as URL-safe base64 of a small JSON object.

Clients must treat a cursor as an opaque string. The position inside is whatever the API puts there,
so long as it is plain JSON.
"""

import base64
import binascii
import json

VERSION = 1


class CursorError(ValueError):
    """A cursor that this code did not make."""


def encode_cursor(position):
    payload = json.dumps({"v": VERSION, "p": position}, separators=(",", ":"), sort_keys=True)
    return base64.urlsafe_b64encode(payload.encode("utf-8")).decode("ascii").rstrip("=")


def decode_cursor(token):
    """The position inside a cursor. Raises CursorError for anything that is not one."""
    if not isinstance(token, str) or not token:
        raise CursorError("invalid cursor")
    try:
        raw = base64.urlsafe_b64decode(token + "=" * (-len(token) % 4))
        payload = json.loads(raw.decode("utf-8"))
    except (binascii.Error, UnicodeDecodeError, json.JSONDecodeError, ValueError):
        raise CursorError("invalid cursor") from None
    if not isinstance(payload, dict) or payload.get("v") != VERSION or "p" not in payload:
        raise CursorError("invalid cursor")
    return payload["p"]
