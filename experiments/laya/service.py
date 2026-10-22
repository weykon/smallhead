"""Local, advisory-only ECS operation selection. No ECS write credentials."""

import argparse
import hashlib
import importlib
import importlib.metadata
import json
import math
import re
import threading
import time
import uuid
from collections import OrderedDict
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

MAX_BODY = 32 * 1024
MAX_STATE_BYTES = 8 * 1024
MAX_QUESTIONS = 16
MAX_OUTCOMES_TRACKED = 2048
DEFAULT_MODEL = "convaiinnovations/laya-multilingual"
DEFAULT_REVISION = "052592a15d198d9ad47da779604259b10b47b7aa"


def load_agent(model_dir, backend="mlx", device=None, dtype=None):
    if backend == "mlx":
        if device not in (None, "gpu", "mps", "cpu"):
            raise ValueError("MLX requires gpu (or mps alias) or cpu")
        import laya_mlx
        return laya_mlx.load(str(model_dir), device="gpu" if device in (None, "mps") else device,
                             dtype=dtype or "float16")
    if backend != "torch":
        raise ValueError("Unknown backend")
    if dtype is not None or device == "gpu":
        raise ValueError("Torch selects its own precision; use device mps, cpu or cuda")
    import laya
    return laya.load(str(model_dir), device=device)


def validate_request(data):
    if not isinstance(data, dict) or set(data) != {"goal", "state", "candidates"}:
        raise ValueError("Expected exactly goal, state and candidates")
    if not isinstance(data["goal"], str) or not 1 <= len(data["goal"].strip()) <= 512:
        raise ValueError("goal must contain 1..512 characters")
    if not isinstance(data["state"], dict):
        raise ValueError("state must be a compact observed JSON object")
    if len(json.dumps(data["state"], ensure_ascii=False).encode()) > 8192:
        raise ValueError("state exceeds 8192 bytes; select relevant observations")
    candidates = data["candidates"]
    if not isinstance(candidates, list) or not 1 <= len(candidates) <= 7:
        raise ValueError("Provide 1..7 candidate operations; defer is added automatically")
    criteria = {}
    for item in candidates:
        if not isinstance(item, dict) or set(item) != {"operation", "description"}:
            raise ValueError("Each candidate needs operation and description")
        op, desc = item["operation"], item["description"]
        if not isinstance(op, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]{0,79}", op):
            raise ValueError("Invalid operation name")
        if op in criteria or op == "defer":
            raise ValueError("Duplicate or reserved operation")
        if not isinstance(desc, str) or not 1 <= len(desc.strip()) <= 256:
            raise ValueError("Candidate description must contain 1..256 characters")
        criteria[op] = desc
    criteria["defer"] = "需要更多证据或推理；这些操作均不适合。"
    return {"operation": {"type": "choice", "instructions": data["goal"], "criteria": criteria}}


def _validate_question(question_id, question):
    if not isinstance(question_id, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_.-]{0,79}", question_id):
        raise ValueError("Invalid question id")
    if not isinstance(question, dict) or not {"type", "instructions"} <= set(question):
        raise ValueError(f"Question {question_id} needs type and instructions")
    if set(question) - {"type", "instructions", "criteria"}:
        raise ValueError(f"Question {question_id} has unknown fields")
    kind = question["type"]
    instructions = question["instructions"]
    if kind not in ("choice", "score", "noul"):
        raise ValueError(f"Question {question_id} has unknown type")
    if not isinstance(instructions, str) or not 1 <= len(instructions.strip()) <= 512:
        raise ValueError(f"Question {question_id} instructions must contain 1..512 characters")
    criteria = question.get("criteria")
    if kind == "choice":
        if not isinstance(criteria, dict) or not 2 <= len(criteria) <= 32:
            raise ValueError(f"Choice {question_id} needs 2..32 criteria")
        for label, description in criteria.items():
            if not isinstance(label, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_.-]{0,79}", label):
                raise ValueError(f"Choice {question_id} has an invalid label")
            if description is not None and (not isinstance(description, str) or len(description) > 256):
                raise ValueError(f"Choice {question_id} descriptions must be short strings or null")
    elif kind == "score":
        if not isinstance(criteria, list) or not 2 <= len(criteria) <= 8:
            raise ValueError(f"Score {question_id} needs 2..8 ordered levels")
        if any(not isinstance(level, str) or not 1 <= len(level.strip()) <= 256 for level in criteria):
            raise ValueError(f"Score {question_id} levels must be short strings")
    elif criteria is not None:
        if not isinstance(criteria, dict) or set(criteria) != {"false", "true"}:
            raise ValueError(f"Noul {question_id} criteria must contain false and true")
        if any(not isinstance(value, str) or not 1 <= len(value.strip()) <= 256 for value in criteria.values()):
            raise ValueError(f"Noul {question_id} criteria must be short strings")


def validate_evaluate_request(data):
    expected = {"spec_version", "spec_id", "spec_hash", "state", "questions"}
    if not isinstance(data, dict) or set(data) != expected:
        raise ValueError("Expected exactly spec_version, spec_id, spec_hash, state and questions")
    if data["spec_version"] != 2:
        raise ValueError("spec_version must be 2")
    if not isinstance(data["spec_id"], str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_.:-]{0,127}", data["spec_id"]):
        raise ValueError("Invalid spec_id")
    if not isinstance(data["spec_hash"], str) or not re.fullmatch(r"[0-9a-f]{64}", data["spec_hash"]):
        raise ValueError("spec_hash must be a lowercase SHA-256 digest")
    if not isinstance(data["state"], dict):
        raise ValueError("state must be a compact observed JSON object")
    if len(json.dumps(data["state"], ensure_ascii=False).encode()) > MAX_STATE_BYTES:
        raise ValueError("state exceeds 8192 bytes; select relevant observations")
    questions = data["questions"]
    if not isinstance(questions, dict) or not 1 <= len(questions) <= MAX_QUESTIONS:
        raise ValueError(f"Provide 1..{MAX_QUESTIONS} questions")
    for question_id, question in questions.items():
        _validate_question(question_id, question)
    return questions


def _internal_question(question):
    return {"t": question["type"], "ins": question["instructions"], "crit": question.get("criteria")}


def check_questions_budget(agent, state, questions, backend="mlx"):
    # Both pinned backends retain the upstream prompt budgets. Import only the selected one.
    common = importlib.import_module("laya_mlx.common" if backend == "mlx" else "laya.common")

    tok = agent.tok
    encode = lambda text: tok(text.replace(tok.mask_token, " "), add_special_tokens=False)["input_ids"]
    state_tokens = len(encode(common.serialize_state(state)))
    total = 0
    for question_id, question in questions.items():
        internal = _internal_question(question)
        options = common.render_options(internal)
        lengths = [len(encode(" " + option)) for option in options]
        if any(n > 48 for n in lengths):
            raise ValueError(f"Question {question_id} exceeds the 48-token option budget")
        option_tokens = sum(n + 1 for n in lengths)
        head_room = int(agent.cfg["head_max_len"]) - option_tokens
        instruction_tokens = len(encode(f"{question['type']} question: " + question["instructions"]))
        if head_room < 16 or instruction_tokens > max(8, head_room):
            raise ValueError(f"Question {question_id} exceeds the head token budget; shorten or split it")
        question_total = instruction_tokens + option_tokens + state_tokens + 4
        if question_total > int(agent.cfg["max_len"]):
            raise ValueError(f"Question {question_id} state exceeds model context; no truncation allowed")
        total += question_total
    return total


def check_token_budget(agent, state, question, backend="mlx"):
    return check_questions_budget(agent, state, {"operation": question}, backend)


def _number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def validate_answers(questions, answers):
    if not isinstance(answers, dict) or set(answers) != set(questions):
        raise RuntimeError("Model answer ids do not match questions")
    for question_id, question in questions.items():
        answer = answers[question_id]
        kind = question["type"]
        if not isinstance(answer, dict) or answer.get("type") not in (None, kind):
            raise RuntimeError(f"Invalid answer type for {question_id}")
        confidence = answer.get("confidence")
        if not _number(confidence) or not 0 <= confidence <= 1:
            raise RuntimeError(f"Invalid confidence for {question_id}")
        if kind == "noul":
            if not _number(answer.get("noul")) or not 0 <= answer["noul"] <= 1:
                raise RuntimeError(f"Invalid noul answer for {question_id}")
            continue
        probabilities = answer.get("probabilities")
        expected = (set(question["criteria"]) if kind == "choice"
                    else {str(index) for index in range(len(question["criteria"]))})
        if not isinstance(probabilities, dict) or set(probabilities) != expected:
            raise RuntimeError(f"Invalid probabilities for {question_id}")
        if any(not _number(value) or not 0 <= value <= 1 for value in probabilities.values()):
            raise RuntimeError(f"Invalid probability value for {question_id}")
        if abs(sum(probabilities.values()) - 1) > 0.01:
            raise RuntimeError(f"Probabilities do not sum to one for {question_id}")
        if kind == "choice" and answer.get("choice") not in expected:
            raise RuntimeError(f"Invalid choice for {question_id}")
        if kind == "score" and (not _number(answer.get("score"))
                                or not 0 <= answer["score"] <= len(question["criteria"]) - 1):
            raise RuntimeError(f"Invalid score for {question_id}")


def validate_outcome(data):
    expected = {"decision_id", "spec_id", "spec_hash", "action_id", "execution_attempted",
                "execution_succeeded", "behavior_verified", "goal_achieved", "verification", "evidence"}
    if not isinstance(data, dict) or set(data) != expected:
        raise ValueError("Invalid outcome fields")
    for key in ("decision_id", "spec_id", "spec_hash"):
        if not isinstance(data[key], str) or not data[key]:
            raise ValueError(f"{key} must be a nonempty string")
    if data["action_id"] is not None and not isinstance(data["action_id"], str):
        raise ValueError("action_id must be a string or null")
    if not isinstance(data["execution_attempted"], bool) or not isinstance(data["behavior_verified"], bool):
        raise ValueError("execution_attempted and behavior_verified must be booleans")
    if data["execution_succeeded"] is not None and not isinstance(data["execution_succeeded"], bool):
        raise ValueError("execution_succeeded must be boolean or null")
    if data["goal_achieved"] is not None and not isinstance(data["goal_achieved"], bool):
        raise ValueError("goal_achieved must be boolean or null")
    if data["behavior_verified"] and (not data["execution_attempted"] or data["execution_succeeded"] is not True):
        raise ValueError("behavior_verified requires a successful execution attempt")
    if data["goal_achieved"] is True and not data["behavior_verified"]:
        raise ValueError("goal_achieved=true requires behavior_verified=true")
    if not isinstance(data["verification"], list) or len(data["verification"]) > 16:
        raise ValueError("verification must contain at most 16 checks")
    if not isinstance(data["evidence"], dict) or len(json.dumps(data["evidence"], ensure_ascii=False).encode()) > 4096:
        raise ValueError("evidence must be a compact object")
    return data


class DecisionService:
    def __init__(self, agent, model, revision, record_dir, backend="mlx"):
        self.agent = agent
        self.model = model
        self.revision = revision
        self.backend = backend
        self.runtime_metadata = {
            "backend": backend,
            "backend_version": importlib.metadata.version("mlx" if backend == "mlx" else "torch"),
            "laya_version": importlib.metadata.version("laya-mlx" if backend == "mlx" else "laya"),
            "precision": str(getattr(agent, "dtype", "unknown")).rsplit(".", 1)[-1],
        }
        self.record_dir = Path(record_dir)
        self.lock = threading.Lock()
        self.record_lock = threading.Lock()
        self.decisions = OrderedDict()
        self.configuration_sha256 = hashlib.sha256(json.dumps(agent.cfg, sort_keys=True).encode()).hexdigest()

    def health(self):
        return {"protocol_version": 1, "status": "ready", "mode": "advisory",
                "model": self.model, "revision": self.revision,
                "device": str(self.agent.device), **self.runtime_metadata,
                "configuration_sha256": self.configuration_sha256,
                "context_tokens": self.agent.cfg["max_len"], "ecs_trained": False,
                "protocol_versions": [1, 2], "question_types": ["choice", "score", "noul"],
                "max_parallel_questions": MAX_QUESTIONS}

    def _remember(self, decision_id, spec_id=None, spec_hash=None):
        with self.record_lock:
            self.decisions[decision_id] = {"spec_id": spec_id, "spec_hash": spec_hash}
            self.decisions.move_to_end(decision_id)
            while len(self.decisions) > MAX_OUTCOMES_TRACKED:
                self.decisions.popitem(last=False)

    def _append_record(self, record):
        self.record_dir.mkdir(parents=True, exist_ok=True)
        path = self.record_dir / (time.strftime("decisions-%Y-%m-%d") + ".jsonl")
        with self.record_lock, path.open("a", encoding="utf-8") as stream:
            stream.write(json.dumps(record, ensure_ascii=False, allow_nan=False) + "\n")

    def _metadata(self):
        return {"mode": "advisory", "executed": False, "behavior_verified": False,
                "model": self.model, "revision": self.revision,
                "configuration_sha256": self.configuration_sha256, **self.runtime_metadata,
                "device": str(self.agent.device), "confidence_calibrated_for_ecs": False,
                "input_truncated": False}

    def predict(self, data):
        questions = validate_request(data)
        if not self.lock.acquire(blocking=False):
            raise BlockingIOError("Model busy; retry later")
        try:
            tokens = check_token_budget(self.agent, data["state"], questions["operation"], self.backend)
            started = time.perf_counter()
            raw = self.agent.predict(data["state"], questions)["answers"]["operation"]
            choices = questions["operation"]["criteria"]
            probs = raw["probabilities"]
            confidence = raw["confidence"]
            if (raw["choice"] not in choices or set(probs) != set(choices)
                    or any(not math.isfinite(p) or not 0 <= p <= 1 for p in probs.values())
                    or abs(sum(probs.values()) - 1) > 0.01
                    or not math.isfinite(confidence) or not 0 <= confidence <= 1):
                raise RuntimeError("Invalid model output")
            result = {"protocol_version": 1, "decision_id": str(uuid.uuid4()), **self._metadata(),
                      "operation": raw["choice"],
                      "confidence": confidence, "confidence_calibrated_for_ecs": False,
                      "probabilities": probs, "input_tokens": tokens, "input_truncated": False,
                      "inference_ms": round((time.perf_counter() - started) * 1000, 2)}
            self._remember(result["decision_id"])
            self._append_record({"kind": "prediction", "timestamp": time.time(), "request": data,
                                 "prediction": result, "verified_outcome": None,
                                 "training_eligible": False})
            return result
        finally:
            self.lock.release()

    def evaluate(self, data):
        questions = validate_evaluate_request(data)
        if not self.lock.acquire(blocking=False):
            raise BlockingIOError("Model busy; retry later")
        try:
            tokens = check_questions_budget(self.agent, data["state"], questions, self.backend)
            started = time.perf_counter()
            answers = self.agent.predict(data["state"], questions)["answers"]
            validate_answers(questions, answers)
            decision_id = str(uuid.uuid4())
            result = {"protocol_version": 2, "decision_id": decision_id,
                      "spec_id": data["spec_id"], "spec_hash": data["spec_hash"],
                      **self._metadata(), "answers": answers, "input_tokens": tokens,
                      "inference_ms": round((time.perf_counter() - started) * 1000, 2)}
            self._remember(decision_id, data["spec_id"], data["spec_hash"])
            self._append_record({"kind": "prediction", "timestamp": time.time(), "request": data,
                                 "prediction": result, "verified_outcome": None,
                                 "training_eligible": False})
            return result
        finally:
            self.lock.release()

    def record_outcome(self, data):
        outcome = validate_outcome(data)
        with self.record_lock:
            known = self.decisions.get(outcome["decision_id"])
        if known is None:
            raise ValueError("Unknown or expired decision_id")
        if known["spec_id"] != outcome["spec_id"] or known["spec_hash"] != outcome["spec_hash"]:
            raise ValueError("Outcome spec identity does not match the decision")
        record = {"kind": "outcome", "timestamp": time.time(), **outcome,
                  "label_provenance": "runtime_readback" if outcome["behavior_verified"] else None,
                  "decision_correct": None, "reviewer": None, "training_eligible": False}
        self._append_record(record)
        return {"recorded": True, "decision_id": outcome["decision_id"],
                "training_eligible": False}


def make_handler(service):
    class Handler(BaseHTTPRequestHandler):
        def setup(self):
            super().setup()
            self.connection.settimeout(15)

        def reply(self, status, data):
            payload = json.dumps(data, ensure_ascii=False, allow_nan=False).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def do_GET(self):
            if self.path == "/health":
                self.reply(200, service.health())
            else:
                self.reply(404, {"error": "Unknown endpoint"})

        def do_POST(self):
            if self.path not in ("/decide", "/evaluate", "/outcome"):
                self.reply(404, {"error": "Unknown endpoint"})
                return
            if self.headers.get("Origin") or self.headers.get("Content-Type", "").split(";")[0] != "application/json":
                self.reply(415, {"error": "Local application/json clients only"})
                return
            try:
                size = int(self.headers.get("Content-Length", "0"))
                if not 0 < size <= MAX_BODY or self.headers.get("Transfer-Encoding"):
                    raise ValueError("Expected a JSON body of at most 32 KiB")
                def reject_constant(value):
                    raise ValueError("Non-finite JSON number: " + value)
                data = json.loads(self.rfile.read(size), parse_constant=reject_constant)
                if self.path == "/decide":
                    result = service.predict(data)
                elif self.path == "/evaluate":
                    result = service.evaluate(data)
                else:
                    result = service.record_outcome(data)
                self.reply(200, result)
            except (ValueError, UnicodeError) as error:
                self.reply(400, {"error": str(error)})
            except BlockingIOError as error:
                self.reply(503, {"error": str(error)})
            except Exception as error:
                print(f"Prediction failed: {type(error).__name__}: {error}", flush=True)
                self.reply(500, {"error": "Local model inference or evidence recording failed; inspect service log"})

        def log_message(self, fmt, *args):
            pass
    return Handler


def argument_parser():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=3031)
    parser.add_argument("--backend", choices=["mlx", "torch"], default="mlx")
    parser.add_argument("--device", choices=["gpu", "mps", "cpu", "cuda"], default=None)
    parser.add_argument("--dtype", choices=["float16", "float32"], default=None,
                        help="MLX precision; defaults to float16")
    parser.add_argument("--record-dir", default="outputs/laya")
    parser.add_argument("--model-dir", type=Path, help="Load an already downloaded checkpoint offline")
    return parser


def main():
    args = argument_parser().parse_args()
    from huggingface_hub import hf_hub_download

    if args.model_dir:
        model_dir = args.model_dir.resolve()
        digest = hashlib.sha256()
        with (model_dir / "model.safetensors").open("rb") as stream:
            for block in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(block)
        revision = "local-sha256:" + digest.hexdigest()
        model = str(model_dir)
    else:
        # Fixed checkpoint manifest avoids fetching other models or mutable main revisions.
        files = ["encoder/config.json", "tokenizer/tokenizer.json", "tokenizer/tokenizer_config.json",
                 "model.safetensors", "rl_agent_config.json"]
        for filename in files:
            path = hf_hub_download(DEFAULT_MODEL, filename, revision=DEFAULT_REVISION)
        model_dir = Path(path).parent
        model, revision = DEFAULT_MODEL, DEFAULT_REVISION
    agent = load_agent(model_dir, args.backend, args.device, args.dtype)
    service = DecisionService(agent, model, revision, args.record_dir, args.backend)
    server = ThreadingHTTPServer(("127.0.0.1", args.port), make_handler(service))
    print(json.dumps(service.health()), flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
