"""Order states, in the order reports list them."""

NONE = "none"
PENDING = "pending"
PAID = "paid"
SHIPPED = "shipped"
DELIVERED = "delivered"
CANCELLED = "cancelled"
REFUNDED = "refunded"
RETURN_PENDING = "return_pending"

# The order of the README's table, by the state each row leads to.
ORDER = (PENDING, PAID, SHIPPED, DELIVERED, CANCELLED, REFUNDED, RETURN_PENDING)

FINAL = frozenset({CANCELLED, REFUNDED})


def is_final(state):
    """No event can move an order out of a final state."""
    return state in FINAL


def rank(state):
    """Where a state comes in reports. NONE comes first."""
    if state == NONE:
        return -1
    return ORDER.index(state)
