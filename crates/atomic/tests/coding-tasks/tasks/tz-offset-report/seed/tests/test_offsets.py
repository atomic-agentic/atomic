import unittest
from datetime import timedelta

from daylog.offsets import format_offset, parse_offset


class OffsetTest(unittest.TestCase):
    def test_east(self):
        self.assertEqual(parse_offset("+05:30"), timedelta(hours=5, minutes=30))
        self.assertEqual(parse_offset("+09:00"), timedelta(hours=9))
        self.assertEqual(parse_offset("+00:00"), timedelta(0))

    def test_west_whole_hours(self):
        self.assertEqual(parse_offset("-05:00"), timedelta(hours=-5))
        self.assertEqual(parse_offset("-12:00"), timedelta(hours=-12))

    def test_bad_text(self):
        for text in ("", "UTC", "+5:30", "05:30", "+0530", "+05:3", "+05:60", "+15:00", None):
            with self.assertRaises(ValueError, msg=repr(text)):
                parse_offset(text)

    def test_format(self):
        self.assertEqual(format_offset(timedelta(hours=5, minutes=30)), "+05:30")
        self.assertEqual(format_offset(timedelta(hours=-5)), "-05:00")
        self.assertEqual(format_offset(timedelta(0)), "+00:00")
