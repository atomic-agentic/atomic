"""The tree view: each directory with its total, and its files, indented by depth."""

from duscan.humanize import format_bytes
from duscan.scanner import directory_sizes


def render_tree(entries, max_depth=None, precision=1, files=True):
    """The tree as text. Directories show the bytes under them; files deeper than max_depth are left out."""
    sizes = directory_sizes(entries)
    lines = [f"{format_bytes(sizes[''], precision):>11}  ."]
    for entry in entries:
        if max_depth is not None and entry.depth > max_depth:
            continue
        indent = "    " * (entry.depth - 1)
        if entry.is_dir:
            lines.append(f"{format_bytes(sizes[entry.path], precision):>11}  {indent}{entry.name}/")
        elif files:
            lines.append(f"{format_bytes(entry.size, precision):>11}  {indent}{entry.name}")
    return "\n".join(lines) + "\n"


def biggest_directories(entries, n=3):
    """The n directories holding the most bytes, as (path, bytes), the root left out."""
    sizes = directory_sizes(entries)
    rows = [(path, size) for path, size in sizes.items() if path]
    return sorted(rows, key=lambda row: (-row[1], row[0]))[:n]
