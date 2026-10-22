import unittest
from unittest.mock import patch

from play import Client, ROOT, load_cases


class ClientTests(unittest.TestCase):
    def test_gold_and_outputs_are_not_sent(self):
        case = load_cases(ROOT / "cases.jsonl")[0]
        client = Client("http://127.0.0.1:3031")
        result = {"operation": "add", "probabilities": {"add": 1, "multiply": 0, "read_order": 0, "defer": 0}}
        with patch.object(client, "request", return_value=result) as request:
            client.decide(case)
        self.assertEqual(set(request.call_args.args[1]), {"goal", "state", "candidates"})

    def test_reject_unknown_selection(self):
        client = Client("http://127.0.0.1:3031")
        with patch.object(client, "request", return_value={"operation": "invented"}):
            with self.assertRaises(ValueError):
                client.decide(load_cases(ROOT / "cases.jsonl")[0])

    def test_reject_nonlocal_urls(self):
        for url in ("https://example.com", "http://127.0.0.1:3031@evil.com", "http://127.0.0.1:3031/?target=remote"):
            with self.assertRaises(ValueError):
                Client(url)


if __name__ == "__main__":
    unittest.main()
