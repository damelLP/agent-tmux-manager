"""Synthetic clock decisions from pi-atm's close/backoff/outbox contract."""
import unittest

from pi_replay import enqueue


class ReplayPolicyTests(unittest.TestCase):
    def test_backoff_boundary_drops_without_touching_pending_frames(self):
        pending = [{"event": "older"}]
        for now in [0, 999]:
            queue, accepted = enqueue(pending, {"event": "outage"}, now, 1000)
            self.assertFalse(accepted)
            self.assertEqual(queue, [{"event": "older"}])
        queue, accepted = enqueue(pending, {"event": "later"}, 1000, 1000)
        self.assertTrue(accepted)
        self.assertEqual(queue, [{"event": "older"}, {"event": "later"}])
        self.assertEqual(pending, [{"event": "older"}], "pure inputs remain unchanged")

    def test_missing_socket_fifo_keeps_latest_256_in_order(self):
        queue = []
        for number in range(300):
            queue, accepted = enqueue(queue, number, 0, 0)
            self.assertTrue(accepted)
        self.assertEqual(queue, list(range(44, 300)))


if __name__ == "__main__":
    unittest.main()
