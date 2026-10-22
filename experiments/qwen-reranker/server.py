"""Exact yes/no-logit scoring with official Qwen weights; loopback only."""
import argparse
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from rerank import PREFIX, SUFFIX


def validate(data):
    if not isinstance(data, dict):
        raise ValueError("Expected JSON object")
    query = data.get("query")
    documents = data.get("documents")
    instruction = data.get("instruction", "根据查询，检索能够回答问题的相关段落")
    if not isinstance(query, str) or not query.strip():
        raise ValueError("query must be a nonempty string")
    if not isinstance(instruction, str) or not instruction.strip():
        raise ValueError("instruction must be a nonempty string")
    if not isinstance(documents, list) or not 1 <= len(documents) <= 32:
        raise ValueError("documents must contain 1..32 strings")
    if any(not isinstance(d, str) or not d.strip() for d in documents):
        raise ValueError("Each document must be a nonempty string")
    return query, documents, instruction


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model-dir", default=".smallhead/qwen-reranker/model")
    parser.add_argument("--port", type=int, default=3032)
    args = parser.parse_args()
    import torch
    from transformers import AutoModelForCausalLM, AutoTokenizer
    device = "mps" if torch.backends.mps.is_available() else "cpu"
    tokenizer = AutoTokenizer.from_pretrained(args.model_dir, padding_side="left", local_files_only=True)
    model = AutoModelForCausalLM.from_pretrained(
        args.model_dir, torch_dtype=torch.float32, attn_implementation="eager", local_files_only=True
    ).to(device).eval()
    yes = tokenizer.convert_tokens_to_ids("yes")
    no = tokenizer.convert_tokens_to_ids("no")
    lock = threading.Lock()
    health = {"model": "Qwen/Qwen3-Reranker-0.6B", "device": device,
              "dtype": "float32", "status": "ready", "max_tokens": 8192,
              "torch": torch.__version__}

    class Handler(BaseHTTPRequestHandler):
        def reply(self, status, data):
            raw = json.dumps(data, ensure_ascii=False, allow_nan=False).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        def do_GET(self):
            self.reply(200 if self.path == "/health" else 404, health if self.path == "/health" else {"error": "not found"})

        def do_POST(self):
            if self.path != "/rerank":
                self.reply(404, {"error": "not found"})
                return
            if self.headers.get("Origin"):
                self.reply(403, {"error": "browser origins not accepted"})
                return
            try:
                length = int(self.headers.get("Content-Length", 0))
                if not 0 < length <= 131072:
                    raise ValueError("Body must be 1..131072 bytes")
                query, documents, instruction = validate(json.loads(self.rfile.read(length)))
            except (ValueError, TypeError) as error:
                self.reply(400, {"error": str(error)})
                return
            if not lock.acquire(blocking=False):
                self.reply(503, {"error": "busy; retry later"})
                return
            try:
                started = time.perf_counter()
                results = []
                for i, doc in enumerate(documents):
                    prompt = PREFIX + f"<Instruct>: {instruction}\n<Query>: {query}\n<Document>: {doc}" + SUFFIX
                    inputs = tokenizer(prompt, return_tensors="pt")
                    count = inputs["input_ids"].shape[1]
                    if count > 8192:
                        raise ValueError("Input exceeds 8192 tokens; shorten it explicitly")
                    with torch.inference_mode():
                        logits = model(**{k: v.to(device) for k, v in inputs.items()}).logits[0, -1, [no, yes]].float()
                        relevance = torch.softmax(logits, dim=0)[1].item()
                    results.append({"index": i, "document": doc, "score": relevance, "input_tokens": count})
                results.sort(key=lambda x: x["score"], reverse=True)
                self.reply(200, {**health, "inference_ms": (time.perf_counter() - started) * 1000,
                                 "results": results, "score_type": "yes_no_softmax_not_cross_candidate_probability"})
            except ValueError as error:
                self.reply(400, {"error": str(error)})
            except Exception as error:
                self.reply(500, {"error": str(error)})
            finally:
                lock.release()

    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(json.dumps(health), flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
