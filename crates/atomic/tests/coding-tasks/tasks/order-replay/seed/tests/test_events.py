import unittest

from orders.events import BadLine, Event, parse_line


class ParseLineTest(unittest.TestCase):
    def test_good(self):
        event = parse_line('{"seq": 7, "ts": "2026-10-04T09:15:02Z", "order": "A-1", "kind": "pay"}')
        self.assertEqual(event, Event(seq=7, ts="2026-10-04T09:15:02Z", order="A-1", kind="pay"))
        self.assertEqual(event.moment.hour, 9)

    def test_round_trip(self):
        event = Event(seq=8, ts="2026-10-04T09:15:03Z", order="A-2", kind="ship")
        self.assertEqual(parse_line(event.to_json()), event)

    def test_bad(self):
        cases = [
            ("{", "not JSON"),
            ("[1]", "not a JSON object"),
            ('{"seq": 1, "ts": "2026-10-04T09:15:02Z", "order": "A-1"}', "missing kind"),
            ('{"seq": "1", "ts": "2026-10-04T09:15:02Z", "order": "A-1", "kind": "pay"}', "seq must be"),
            ('{"seq": 1, "ts": "2026-10-04 09:15:02", "order": "A-1", "kind": "pay"}', "not a timestamp"),
            ('{"seq": 1, "ts": "2026-10-04T09:15:02Z", "order": "", "kind": "pay"}', "order must be"),
            ('{"seq": 1, "ts": "2026-10-04T09:15:02Z", "order": "A-1", "kind": "lose"}', "unknown event kind"),
        ]
        for text, message in cases:
            with self.assertRaisesRegex(BadLine, message):
                parse_line(text, "x.jsonl:3")


if __name__ == "__main__":
    unittest.main()
