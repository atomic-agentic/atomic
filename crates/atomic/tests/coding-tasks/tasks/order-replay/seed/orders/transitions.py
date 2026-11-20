"""The order state machine: which event moves an order from which state to which."""

from . import states

KINDS = ("create", "pay", "ship", "deliver", "cancel", "refund", "return")

TABLE = {
    (states.NONE, "create"): states.PENDING,
    (states.PENDING, "pay"): states.PAID,
    (states.PAID, "ship"): states.SHIPPED,
    (states.SHIPPED, "deliver"): states.DELIVERED,
    (states.PENDING, "cancel"): states.CANCELLED,
    (states.PAID, "cancel"): states.CANCELLED,
    (states.PAID, "refund"): states.REFUNDED,
    (states.DELIVERED, "return"): states.RETURN_PENDING,
    (states.RETURN_PENDING, "refund"): states.REFUNDED,
}


class InvalidTransition(Exception):
    def __init__(self, state, kind):
        super().__init__(f"{kind} from {state}")
        self.state = state
        self.kind = kind


def is_kind(kind):
    return kind in KINDS


def next_state(state, kind):
    """The state after an event, or InvalidTransition."""
    try:
        return TABLE[(state, kind)]
    except KeyError:
        raise InvalidTransition(state, kind) from None


def allowed(state):
    """The events allowed in a state, in KINDS order."""
    return [kind for kind in KINDS if (state, kind) in TABLE]


def table_rows():
    """(from, event, to) rows, as the README's table has them."""
    return [(src, kind, dst) for (src, kind), dst in TABLE.items()]
