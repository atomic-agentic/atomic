"""Timestamps for lines: UTC, to the second."""

from datetime import datetime, timezone


def format_timestamp(epoch_seconds):
    moment = datetime.fromtimestamp(epoch_seconds, tz=timezone.utc)
    return moment.strftime("%Y-%m-%dT%H:%M:%SZ")
