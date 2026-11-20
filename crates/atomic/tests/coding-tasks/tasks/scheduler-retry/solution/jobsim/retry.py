"""When a failed attempt is tried again: exponential backoff, with a cap."""

from dataclasses import dataclass


@dataclass(frozen=True)
class RetryPolicy:
    base: int = 5  # seconds before the second attempt
    factor: int = 2  # each further wait is this many times the one before
    max_delay: int = 300  # no wait is longer than this

    def delay(self, number):
        """The wait after attempt `number` fails: base, base * factor, base * factor ** 2, ..."""
        if number < 1:
            raise ValueError("attempts are counted from 1")
        return min(self.base * self.factor ** (number - 1), self.max_delay)

    def next_run(self, attempt, failed_at):
        """When to run the attempt after `attempt`, which failed at `failed_at`."""
        return failed_at + self.delay(attempt.number)

    def schedule(self, number):
        """The waits after attempts 1 to number, for documentation and checks."""
        return [self.delay(n) for n in range(1, number + 1)]
