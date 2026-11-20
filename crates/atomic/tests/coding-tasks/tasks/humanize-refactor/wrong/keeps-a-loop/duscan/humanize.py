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


def _scale(value, base, units, precision, strict=False, whole=False):
    """(number, unit) for a value that is not negative: divided by base while it is at least base (more than
    base when strict), up to the last unit, then given precision decimals. With whole, an int that needed no
    dividing is printed as it is. The check comes before rounding, so 1023.96 bytes stays in bytes."""
    scaled = value
    index = 0
    while (scaled > base if strict else scaled >= base) and index < len(units) - 1:
        scaled = scaled / base
        index += 1
    if index == 0 and whole and isinstance(value, int):
        return f"{value}", units[0]
    return f"{scaled:.{precision}f}", units[index]


def format_bytes(size, precision=1):
    """A size in bytes, in binary units: 1536 is '1.5 KiB', and 512 is '512 B'."""
    _check_number("format_bytes", size)
    if size == 0:
        return "0 B"
    sign = "-" if size < 0 else ""
    number, unit = _scale(abs(size), 1024, BINARY_UNITS, precision, whole=True)
    return f"{sign}{number} {unit}"


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
    number, unit = _scale(float(bytes_per_second), 1000, RATE_UNITS, precision, strict=True)
    return f"{number} {unit}"


def format_share(part, whole):
    """part as a percentage of whole, to one decimal: '12.5%'. Nothing of nothing is '0.0%'."""
    if whole == 0:
        return "0.0%"
    return f"{100.0 * part / whole:.1f}%"
