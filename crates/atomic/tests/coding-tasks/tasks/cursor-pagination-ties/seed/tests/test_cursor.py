import base64
import unittest

from tickets.cursor import CursorError, decode_cursor, encode_cursor


class CursorTest(unittest.TestCase):
    def test_round_trip(self):
        for position in ("2026-05-04T09:00:00Z", {"page": 3}, 17):
            token = encode_cursor(position)
            self.assertIsInstance(token, str)
            self.assertNotIn("=", token)
            self.assertEqual(decode_cursor(token), position)

    def test_url_safe(self):
        token = encode_cursor("??>>~~" * 5)
        self.assertTrue(all(c.isalnum() or c in "-_" for c in token), token)

    def test_rejects_what_it_did_not_make(self):
        foreign = base64.urlsafe_b64encode(b'{"v": 0, "p": 1}').decode()
        for token in ("", "not a cursor", "!!!!", foreign, None, 5):
            with self.assertRaises(CursorError, msg=repr(token)):
                decode_cursor(token)
        self.assertTrue(issubclass(CursorError, ValueError))
