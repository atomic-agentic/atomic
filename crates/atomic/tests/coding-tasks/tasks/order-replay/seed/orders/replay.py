"""Rebuilding each order's state from its events."""

from dataclasses import dataclass, field

from . import states, timeutil, transitions


@dataclass(frozen=True)
class Problem:
    seq: int
    order: str
    kind: str
    state: str  # the state the order was in when the event came

    def describe(self):
        return f"seq {self.seq} order {self.order} {self.kind} from {self.state}"


@dataclass
class Result:
    states: dict = field(default_factory=dict)  # order id -> final state
    problems: list = field(default_factory=list)  # Problems, in the order they were found
    events: int = 0
    first_ts: str = ""
    last_ts: str = ""

    @property
    def ok(self):
        return not self.problems


def replay(events):
    """Apply events, which must already be in order, to orders that start in states.NONE."""
    result = Result()
    moments = []
    for event in events:
        result.events += 1
        moments.append(event.moment)
        state = result.states.get(event.order, states.NONE)
        try:
            result.states[event.order] = transitions.next_state(state, event.kind)
        except transitions.InvalidTransition:
            result.states.setdefault(event.order, state)
            result.problems.append(Problem(seq=event.seq, order=event.order, kind=event.kind, state=state))
    if moments:
        first, last = timeutil.span(moments)
        result.first_ts, result.last_ts = timeutil.format(first), timeutil.format(last)
    return result


def counts(result):
    """(state, number of orders) for each state that has any, in states.ORDER order."""
    tally = {}
    for state in result.states.values():
        tally[state] = tally.get(state, 0) + 1
    return [(state, tally[state]) for state in sorted(tally, key=states.rank)]
