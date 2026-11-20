"""The collector's own acceptance rules, so lines can be checked before they are shipped."""


class Collector:
    def __init__(self, max_line_bytes):
        self.max_line_bytes = max_line_bytes

    def check(self, data):
        """Returns None if the collector accepts the line (bytes, without its newline), else why not."""
        if b"\n" in data or b"\r" in data:
            return "holds a line break"
        try:
            data.decode("utf-8", errors="strict")
        except UnicodeDecodeError:
            return "not valid UTF-8"
        if len(data) > self.max_line_bytes:
            return f"too long ({len(data)} bytes, limit {self.max_line_bytes})"
        return None
