"""Human-readable numbers for duscan's reports: sizes, counts and rates.

Scripts parse these strings, so they must not change from one release to the next.
"""

BINARY_UNITS = ("B", "KiB", "MiB", "GiB", "TiB", "PiB")
SI_PREFIXES = ("", "k", "M", "G", "T", "P")
RATE_UNITS = ("B/s", "kB/s", "MB/s", "GB/s", "TB/s")


def _check_number(name, value):
    if value is None:
        raise TypeError(f"{name}() needs a number, not None")
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise TypeError(f"{name}() needs a number, not {type(value).__name__}")


def format_bytes(size, precision=1):
    """A size in bytes, in binary units: 1536 is '1.5 KiB', and 512 is '512 B'."""
    _check_number("format_bytes", size)
    if size == 0:
        return "0 B"
    sign = "-" if size < 0 else ""
    value = abs(size)
    index = 0
    while value >= 1024 and index < len(BINARY_UNITS) - 1:
        value = value / 1024
        index += 1
    if index == 0 and isinstance(size, int):
        return f"{sign}{value} B"
    return f"{sign}{value:.{precision}f} {BINARY_UNITS[index]}"


def format_si(count, precision=1):
    """A count, with an SI prefix and no space: 1500 is '1.5k', and 42 is '42'."""
    _check_number("format_si", count)
    if count == 0:
        return "0"
    sign = "-" if count < 0 else ""
    value = abs(count)
    index = 0
    while value >= 1000 and index < len(SI_PREFIXES) - 1:
        value = value / 1000
        index += 1
    if index == 0:
        if isinstance(count, int):
            return f"{sign}{value}"
        return f"{sign}{value:.{precision}f}"
    return f"{sign}{value:.{precision}f}{SI_PREFIXES[index]}"


def format_rate(bytes_per_second, precision=1):
    """A throughput in decimal units: 2500000 is '2.5 MB/s'."""
    _check_number("format_rate", bytes_per_second)
    if bytes_per_second < 0:
        raise ValueError(f"a rate cannot be negative: {bytes_per_second}")
    if bytes_per_second == 0:
        return "0 B/s"
    value = float(bytes_per_second)
    index = 0
    while value > 1000 and index < len(RATE_UNITS) - 1:
        value = value / 1000
        index += 1
    return f"{value:.{precision}f} {RATE_UNITS[index]}"


def format_share(part, whole):
    """part as a percentage of whole, to one decimal: '12.5%'. Nothing of nothing is '0.0%'."""
    if whole == 0:
        return "0.0%"
    return f"{100.0 * part / whole:.1f}%"
