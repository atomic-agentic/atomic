"""The scheduler's queue of attempts, on a binary heap."""

import heapq
import itertools

from .jobs import Attempt


class EventQueue:
    """Attempts waiting to run. pop() gives the one to run next: the earliest due, then the highest
    priority among those due at the same moment, then the one queued first."""

    def __init__(self):
        self._heap = []
        self._seq = itertools.count()

    def push(self, job, number, run_at):
        """Queue an attempt at a job, due at run_at. Returns the Attempt."""
        attempt = Attempt(job=job, number=number, run_at=run_at, seq=next(self._seq))
        # The queueing order breaks any remaining tie, and is unique, so attempts are never compared.
        heapq.heappush(self._heap, (run_at, -job.priority, attempt.seq, attempt))
        return attempt

    def pop(self):
        """The next attempt to run, removed from the queue."""
        return heapq.heappop(self._heap)[-1]

    def peek(self):
        return self._heap[0][-1] if self._heap else None

    def __len__(self):
        return len(self._heap)

    def __bool__(self):
        return bool(self._heap)

    def pending(self):
        """The queued attempts, in the order pop() would give them, without removing any."""
        return [entry[-1] for entry in sorted(self._heap)]
