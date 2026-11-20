"""Writes formatted lines to the file the shipping agent tails, checking each one first."""

from dataclasses import dataclass, field

from logship.collector import Collector


@dataclass
class Report:
    sent: int = 0
    rejected: list = field(default_factory=list)


class BatchSender:
    def __init__(self, path, formatter):
        self.path = path
        self.formatter = formatter
        self.collector = Collector(formatter.max_line_bytes)

    def send(self, numbered_records):
        """Writes every line, rejected or not, and reports which ones the collector would refuse."""
        report = Report()
        with open(self.path, "wb") as out:
            for number, record in numbered_records:
                data = self.formatter.format(record).encode("utf-8")
                reason = self.collector.check(data)
                if reason is not None:
                    report.rejected.append((number, reason))
                out.write(data + b"\n")
                report.sent += 1
        return report
