"""Simulated time, in whole seconds from the start of the scenario."""


class ClockError(Exception):
    pass


class SimClock:
    def __init__(self, now=0):
        self._now = now

    @property
    def now(self):
        return self._now

    def advance_to(self, moment):
        """Move to a later moment. Time never runs backwards."""
        if moment < self._now:
            raise ClockError(f"cannot go back from t={self._now} to t={moment}")
        self._now = moment

    def advance(self, seconds):
        if seconds < 0:
            raise ClockError(f"cannot advance by {seconds} s")
        self._now += seconds

    def __repr__(self):
        return f"SimClock(now={self._now})"
