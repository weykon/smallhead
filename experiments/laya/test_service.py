import json
import tempfile
import threading
import unittest
from http.server import ThreadingHTTPServer
from unittest.mock import Mock, patch
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from service import (DecisionService, make_handler, validate_request, validate_evaluate_request,
                     check_token_budget, check_questions_budget, argument_parser, load_agent)


def request():
    return {"goal": "更新计数器", "state": {"count": 0}, "candidates": [
        {"operation": "modify_component", "description": "更新组件数据"}]}


class FakeAgent:
    device = "cpu"
    cfg = {"max_len": 1024, "head_max_len": 256}

    def predict(self, state, questions):
        answers = {}
        for question_id, question in questions.items():
            if question["type"] == "choice":
                choice = next(iter(question["criteria"]))
                count = len(question["criteria"])
                answers[question_id] = {"type": "choice", "choice": choice, "confidence": 0.9,
                    "probabilities": {key: (0.9 if key == choice else 0.1 / (count - 1))
                                      for key in question["criteria"]}}
            elif question["type"] == "score":
                answers[question_id] = {"type": "score", "score": 0.2, "confidence": 0.8,
                    "probabilities": {"0": 0.8, "1": 0.2}, "legend": {"0": "low", "1": "high"}}
            else:
                answers[question_id] = {"type": "noul", "noul": 0.9, "confidence": 0.9}
        return {"answers": answers}


class ServiceTests(unittest.TestCase):
    def test_default_backend_is_mlx_and_torch_is_explicit(self):
        self.assertEqual(argument_parser().parse_args([]).backend, "mlx")
        mlx, torch = Mock(), Mock()
        with patch.dict("sys.modules", {"laya_mlx": mlx, "laya": torch}):
            load_agent("checkpoint")
            mlx.load.assert_called_once_with("checkpoint", device="gpu", dtype="float16")
            torch.load.assert_not_called()
            load_agent("checkpoint", backend="torch", device="mps")
            torch.load.assert_called_once_with("checkpoint", device="mps")
            with self.assertRaises(ValueError):
                load_agent("checkpoint", device="cuda")

    def test_validation_rejects_empty_duplicate_and_nonobject(self):
        for value in [None, {}, {**request(), "state": "snapshot"},
                      {**request(), "candidates": request()["candidates"] * 2},
                      {**request(), "goal": " "}, {**request(), "extra": 1}]:
            with self.assertRaises(ValueError):
                validate_request(value)

    def test_http_records_advice_without_success_label(self):
        with tempfile.TemporaryDirectory() as directory, patch("service.check_token_budget", return_value=50):
            service = DecisionService(FakeAgent(), "fake", "revision", directory)
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(service))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                url = f"http://127.0.0.1:{server.server_port}/decide"
                req = Request(url, json.dumps(request()).encode(), {"Content-Type": "application/json"})
                with urlopen(req, timeout=5) as stream:
                    result = json.load(stream)
                self.assertFalse(result["executed"])
                self.assertEqual(result["backend"], "mlx")
                self.assertFalse(result["confidence_calibrated_for_ecs"])
                records = list(service.record_dir.glob("*.jsonl"))
                record = json.loads(records[0].read_text())
                self.assertIsNone(record["verified_outcome"])
                self.assertFalse(record["training_eligible"])
                req = Request(url, b"{}", {"Content-Type": "application/json"})
                with self.assertRaises(HTTPError) as error:
                    urlopen(req, timeout=5)
                self.assertEqual(error.exception.code, 400)
                self.assertEqual(len(records[0].read_text().splitlines()), 1)
            finally:
                server.shutdown()
                server.server_close()
                thread.join()

    def test_token_budgets_reject_clipping(self):
        class Tokenizer:
            mask_token = "[MASK]"
            def __call__(self, text, **kwargs):
                return {"input_ids": text.split()}
        agent = FakeAgent()
        agent.tok = Tokenizer()
        question = validate_request(request())["operation"]
        self.assertLess(check_token_budget(agent, request()["state"], question), 1024)
        with self.assertRaisesRegex(ValueError, "context"):
            check_token_budget(agent, {"text": "word " * 1100}, question)
        question["criteria"]["modify_component"] = "word " * 60
        with self.assertRaisesRegex(ValueError, "option budget"):
            check_token_budget(agent, {}, question)

    def test_v2_parallel_questions_and_outcome_are_linked(self):
        evaluation = {"spec_version": 2, "spec_id": "counter.v1", "spec_hash": "a" * 64,
            "state": {"count": 0}, "questions": {
                "action": {"type": "choice", "instructions": "选择下一步动作",
                           "criteria": {"modify": "set the counter", "defer": "wait"}},
                "evidence": {"type": "noul", "instructions": "证据是否充分？"},
                "risk": {"type": "score", "instructions": "风险程度如何？",
                         "criteria": ["low", "high"]}}}
        self.assertEqual(len(validate_evaluate_request(evaluation)), 3)
        with tempfile.TemporaryDirectory() as directory, patch("service.check_questions_budget", return_value=90):
            service = DecisionService(FakeAgent(), "fake", "revision", directory)
            result = service.evaluate(evaluation)
            self.assertEqual(result["protocol_version"], 2)
            self.assertEqual(set(result["answers"]), {"action", "evidence", "risk"})
            outcome = {"decision_id": result["decision_id"], "spec_id": "counter.v1",
                "spec_hash": "a" * 64, "action_id": "modify", "execution_attempted": True,
                "execution_succeeded": True, "behavior_verified": True, "goal_achieved": True,
                "verification": [{"id": "counter", "passed": True}], "evidence": {"count": 1}}
            self.assertTrue(service.record_outcome(outcome)["recorded"])
            records = [json.loads(line) for line in next(service.record_dir.glob("*.jsonl")).read_text().splitlines()]
            self.assertEqual([record["kind"] for record in records], ["prediction", "outcome"])
            self.assertEqual(records[1]["label_provenance"], "runtime_readback")
            self.assertFalse(records[1]["training_eligible"])

    def test_v2_validation_rejects_unsafe_or_ambiguous_shapes(self):
        base = {"spec_version": 2, "spec_id": "test", "spec_hash": "b" * 64,
                "state": {}, "questions": {"ready": {"type": "noul", "instructions": "是否就绪？"}}}
        validate_evaluate_request(base)
        invalid = json.loads(json.dumps(base))
        invalid["questions"]["ready"]["criteria"] = {"yes": "yes", "no": "no"}
        with self.assertRaisesRegex(ValueError, "false and true"):
            validate_evaluate_request(invalid)
        invalid = json.loads(json.dumps(base))
        invalid["questions"]["action"] = {"type": "choice", "instructions": "请选择", "criteria": {"only": "one"}}
        with self.assertRaisesRegex(ValueError, "2..32"):
            validate_evaluate_request(invalid)


if __name__ == "__main__":
    unittest.main()
