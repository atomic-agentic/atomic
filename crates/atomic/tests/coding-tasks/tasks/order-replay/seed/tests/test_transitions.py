import unittest

from orders import states, transitions


class TransitionsTest(unittest.TestCase):
    def test_happy_path(self):
        state = states.NONE
        for kind, expected in [("create", "pending"), ("pay", "paid"), ("ship", "shipped"), ("deliver", "delivered"),
                               ("return", "return_pending"), ("refund", "refunded")]:
            state = transitions.next_state(state, kind)
            self.assertEqual(state, expected)

    def test_cancel(self):
        self.assertEqual(transitions.next_state("pending", "cancel"), "cancelled")
        self.assertEqual(transitions.next_state("paid", "cancel"), "cancelled")
        with self.assertRaises(transitions.InvalidTransition):
            transitions.next_state("shipped", "cancel")

    def test_invalid(self):
        with self.assertRaises(transitions.InvalidTransition) as caught:
            transitions.next_state("pending", "ship")
        self.assertEqual(str(caught.exception), "ship from pending")
        for state in states.FINAL:
            self.assertEqual(transitions.allowed(state), [])

    def test_allowed(self):
        self.assertEqual(transitions.allowed("paid"), ["ship", "cancel", "refund"])
        self.assertEqual(transitions.allowed(states.NONE), ["create"])


if __name__ == "__main__":
    unittest.main()
