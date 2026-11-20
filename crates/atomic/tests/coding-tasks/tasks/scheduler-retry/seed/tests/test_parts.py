import unittest

from jobsim.clock import ClockError, SimClock
from jobsim.jobs import Job
from jobsim.queue import EventQueue
from jobsim.retry import RetryPolicy


class ClockTest(unittest.TestCase):
    def test_forward_only(self):
        clock = SimClock()
        clock.advance_to(5)
        clock.advance(3)
        self.assertEqual(clock.now, 8)
        with self.assertRaises(ClockError):
            clock.advance_to(7)
        with self.assertRaises(ClockError):
            clock.advance(-1)


class RetryPolicyTest(unittest.TestCase):
    def test_delays(self):
        policy = RetryPolicy(base=5, factor=2, max_delay=60)
        self.assertEqual(policy.schedule(6), [5, 10, 20, 40, 60, 60])

    def test_constant(self):
        self.assertEqual(RetryPolicy(base=7, factor=1).schedule(3), [7, 7, 7])

    def test_counted_from_one(self):
        with self.assertRaises(ValueError):
            RetryPolicy().delay(0)


class QueueTest(unittest.TestCase):
    def test_earliest_first(self):
        queue = EventQueue()
        queue.push(Job(name="late", at=9), 1, 9)
        queue.push(Job(name="early", at=1), 1, 1)
        queue.push(Job(name="middle", at=4), 1, 4)
        self.assertEqual([queue.pop().job.name for _ in range(3)], ["early", "middle", "late"])
        self.assertFalse(queue)

    def test_priority_at_the_same_moment(self):
        queue = EventQueue()
        queue.push(Job(name="a", at=2, priority=1), 1, 2)
        queue.push(Job(name="b", at=2, priority=5), 1, 2)
        queue.push(Job(name="c", at=1, priority=0), 1, 1)
        self.assertEqual([a.job.name for a in queue.pending()], ["c", "b", "a"])
        self.assertEqual(len(queue), 3)
        self.assertEqual(queue.peek().job.name, "c")

    def test_attempts_are_numbered_in_queue_order(self):
        queue = EventQueue()
        first = queue.push(Job(name="x", at=0), 1, 0)
        second = queue.push(Job(name="y", at=0), 1, 3)
        self.assertEqual((first.seq, second.seq), (0, 1))


class JobTest(unittest.TestCase):
    def test_outcomes(self):
        job = Job(name="j", at=0, outcomes=("fail", "fail"))
        self.assertEqual([job.outcome(n) for n in (1, 2, 3, 4)], ["fail", "fail", "ok", "ok"])


if __name__ == "__main__":
    unittest.main()
