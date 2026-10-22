import math
import unittest
from rerank import probability


class ScoreTests(unittest.TestCase):
    def test_normalizes_only_yes_no(self):
        score, _ = probability([{"top_logprobs": [
            {"token": "yes", "logprob": math.log(.6)},
            {"token": "no", "logprob": math.log(.2)},
            {"token": "other", "logprob": math.log(.2)}]}])
        self.assertAlmostEqual(score, .75)

    def test_missing_logits_are_not_a_zero_score(self):
        with self.assertRaises(ValueError):
            probability([])

    def test_extreme_difference_is_stable(self):
        score, _ = probability([{"top_logprobs": [
            {"token": "yes", "logprob": -1000}, {"token": "no", "logprob": 0}]}])
        self.assertEqual(score, 0)
