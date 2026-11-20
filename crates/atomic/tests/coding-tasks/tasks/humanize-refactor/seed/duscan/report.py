"""The summary report: totals, the largest files, and sizes by extension."""

from duscan.humanize import format_bytes, format_rate, format_share, format_si


def extension_of(path):
    """'.py' for 'a/b.py', '' for 'Makefile' and for '.bashrc'. Lower case."""
    name = path.rsplit("/", 1)[-1]
    if "." not in name.lstrip("."):
        return ""
    return "." + name.rsplit(".", 1)[-1].lower()


def by_extension(entries):
    """(extension, file count, bytes) for each extension, biggest first, then by name."""
    groups = {}
    for entry in entries:
        if entry.is_dir:
            continue
        count, size = groups.get(extension_of(entry.path), (0, 0))
        groups[extension_of(entry.path)] = (count + 1, size + entry.size)
    return sorted(((ext, c, s) for ext, (c, s) in groups.items()), key=lambda row: (-row[2], row[0]))


def largest(entries, n):
    files = [e for e in entries if not e.is_dir]
    return sorted(files, key=lambda e: (-e.size, e.path))[:n]


def summary(entries, top=5, seconds=None, extensions=8):
    """The report as text. With seconds, the time the scan took, it ends with the scan's throughput."""
    files = [e for e in entries if not e.is_dir]
    total = sum(e.size for e in files)
    directories = sum(1 for e in entries if e.is_dir)
    lines = [f"files: {format_si(len(files))}  directories: {format_si(directories)}  total: {format_bytes(total)}"]
    if files:
        lines.append("")
        lines.append(f"largest {min(top, len(files))}:")
        for entry in largest(entries, top):
            lines.append(f"  {format_bytes(entry.size):>10}  {entry.path}")
        lines.append("")
        lines.append("by extension:")
        rows = by_extension(entries)
        for ext, count, size in rows[:extensions]:
            label = ext or "(none)"
            lines.append(f"  {label:<8} {format_si(count):>6}  {format_bytes(size):>10}  {format_share(size, total):>6}")
        if len(rows) > extensions:
            rest = rows[extensions:]
            count = sum(r[1] for r in rest)
            size = sum(r[2] for r in rest)
            lines.append(f"  {'(other)':<8} {format_si(count):>6}  {format_bytes(size):>10}  {format_share(size, total):>6}")
    if seconds is not None and seconds > 0:
        lines.append("")
        lines.append(f"scanned at {format_rate(total / seconds, precision=2)}")
    return "\n".join(lines) + "\n"
