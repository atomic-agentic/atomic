"""The text the commands print."""

from . import replay, transitions


def replay_text(result):
    lines = [f"{order} {result.states[order]}" for order in sorted(result.states)]
    lines += [f"invalid: {problem.describe()}" for problem in result.problems]
    noun = "order" if len(result.states) == 1 else "orders"
    events = "event" if result.events == 1 else "events"
    lines.append(f"{len(result.states)} {noun}, {result.events} {events}, {len(result.problems)} invalid")
    return "\n".join(lines) + "\n"


def summary_text(result):
    lines = [f"{state} {count}" for state, count in replay.counts(result)]
    noun = "order" if len(result.states) == 1 else "orders"
    if result.events:
        lines.append(f"{len(result.states)} {noun}, from {result.first_ts} to {result.last_ts}")
    else:
        lines.append("0 orders")
    return "\n".join(lines) + "\n"


def states_text():
    rows = [("From", "Event", "To")] + transitions.table_rows()
    widths = [max(len(row[i]) for row in rows) for i in range(3)]
    return "\n".join("  ".join(cell.ljust(width) for cell, width in zip(row, widths)).rstrip() for row in rows) + "\n"
