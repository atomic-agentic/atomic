"""Records to lines: `<PRI>TIMESTAMP HOST APP: MESSAGE`, within the collector's limit."""

from logship.textutil import clean, token, truncate_bytes, utf8_len
from logship.timefmt import format_timestamp

ELLIPSIS = "..."


class Formatter:
    def __init__(self, config):
        self.config = config
        self.max_line_bytes = config.max_line_bytes

    def priority(self, record):
        return self.config.facility * 8 + record.severity_number

    def prefix(self, record):
        host = token(record.host if record.host is not None else self.config.host)
        app = token(record.app if record.app is not None else self.config.app)
        return f"<{self.priority(record)}>{format_timestamp(record.time)} {host} {app}: "

    def format(self, record):
        prefix = self.prefix(record)
        message = clean(record.message)
        line = prefix + message
        if utf8_len(line) <= self.max_line_bytes:
            return line
        room = self.max_line_bytes - utf8_len(prefix) - utf8_len(ELLIPSIS)
        if room < 0:
            raise ValueError("the line prefix leaves no room for a message")
        return prefix + truncate_bytes(message, room) + ELLIPSIS

    def format_all(self, records):
        return [self.format(record) for record in records]
