"""The simulation: one worker taking attempts off the queue until it is empty."""

from .clock import SimClock
from .jobs import FAIL, OK
from .queue import EventQueue
from .trace import Trace


class Simulator:
    def __init__(self, scenario):
        self.scenario = scenario
        self.clock = SimClock()
        self.queue = EventQueue()
        self.trace = Trace()

    def run(self):
        """Run every job to success or to its last attempt, and return the trace."""
        for job in self.scenario.jobs:
            self.queue.push(job, 1, job.at)
        while self.queue:
            self.step()
        self.trace.finish(self.clock.now)
        return self.trace

    def step(self):
        attempt = self.queue.pop()
        # One worker: an attempt that is due while another runs waits for it to finish.
        start = max(self.clock.now, attempt.run_at)
        self.clock.advance_to(start)
        outcome = attempt.job.outcome(attempt.number)
        self.trace.run(start, attempt, outcome)
        self.clock.advance(attempt.job.duration)
        if outcome == OK:
            self.trace.succeeded()
        elif outcome == FAIL:
            self.failed(attempt)

    def failed(self, attempt):
        failed_at = self.clock.now
        if attempt.is_last:
            self.trace.give_up(failed_at, attempt)
            return
        run_at = self.scenario.retry.next_run(attempt, failed_at)
        self.queue.push(attempt.job, attempt.number + 1, run_at)


def simulate(scenario):
    """The trace of a scenario, as text."""
    return Simulator(scenario).run().text()
