"""Jobs, as a scenario describes them, and the attempts the scheduler makes at them."""

from dataclasses import dataclass

OK = "ok"
FAIL = "fail"


@dataclass(frozen=True)
class Job:
    name: str
    at: int  # when it is first due, in seconds
    priority: int = 0  # higher runs first among jobs due at the same moment
    duration: int = 1  # seconds each attempt takes, whether it succeeds or fails
    outcomes: tuple = ()  # the outcome of attempt 1, 2, ...; attempts past the end succeed
    max_attempts: int = 3

    def outcome(self, attempt):
        """OK or FAIL for an attempt, counted from 1."""
        if attempt <= len(self.outcomes):
            return self.outcomes[attempt - 1]
        return OK


@dataclass(frozen=True)
class Attempt:
    """One attempt waiting in the queue."""

    job: Job
    number: int  # 1 for the first attempt
    run_at: int  # when it is due
    seq: int  # the order attempts were put in the queue, from 0

    @property
    def is_last(self):
        return self.number >= self.job.max_attempts
