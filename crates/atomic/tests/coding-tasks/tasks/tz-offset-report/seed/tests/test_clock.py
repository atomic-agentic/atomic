import unittest
from datetime import date, datetime, timedelta, timezone

from daylog.clock import local_day, parse_utc, to_local


class ClockTest(unittest.TestCase):
    def test_parse(self):
        self.assertEqual(parse_utc("2026-03-02T03:10:00Z"), datetime(2026, 3, 2, 3, 10, tzinfo=timezone.utc))
        self.assertEqual(parse_utc("2026-03-02T03:10:00+00:00"), datetime(2026, 3, 2, 3, 10, tzinfo=timezone.utc))
        for text in ("2026-03-02T03:10:00", "2026-03-02T03:10Z", "2026-03-02 03:10:00Z", "2026-03-02T03:10:00+01:00"):
            with self.assertRaises(ValueError, msg=text):
                parse_utc(text)

    def test_local_day_east(self):
        moment = parse_utc("2026-03-01T18:30:00Z")
        self.assertEqual(local_day(moment, timedelta(hours=5, minutes=30)), date(2026, 3, 2))
        self.assertEqual(local_day(moment, timedelta(hours=5, minutes=29)), date(2026, 3, 1))

    def test_local_day_west(self):
        moment = parse_utc("2026-03-02T04:30:00Z")
        self.assertEqual(local_day(moment, timedelta(hours=-5)), date(2026, 3, 1))
        self.assertEqual(local_day(moment, timedelta(hours=-4)), date(2026, 3, 2))

    def test_to_local(self):
        local = to_local(parse_utc("2026-03-02T04:30:00Z"), timedelta(hours=-5))
        self.assertEqual((local.day, local.hour, local.minute), (1, 23, 30))
        with self.assertRaises(ValueError):
            to_local(datetime(2026, 3, 2), timedelta(0))
