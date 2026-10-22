"""Local Qwen3 reranking via Ollama's raw generate + first-token logprobs."""
import argparse
import json
import math
import time
from urllib.error import URLError
from urllib.request import Request, ProxyHandler, build_opener

MODEL = "ExpedientFalcon/qwen3-reranker:0.6b-q8_0"
PREFIX = "<|im_start|>system\n根据提供的 Query 和 Instruct，判断 Document 是否符合要求。答案只能是 \"yes\" 或 \"no\"。<|im_end|>\n<|im_start|>user\n"
SUFFIX = "<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"


def request(path, payload, base="http://127.0.0.1:11434"):
    req = Request(base + path, json.dumps(payload).encode(), {"Content-Type": "application/json"})
    with build_opener(ProxyHandler({})).open(req, timeout=180) as response:
        return json.load(response)


def probability(entries):
    if not entries:
        raise ValueError("Ollama did not return logprobs; cannot compute a genuine reranker score")
    values = {item["token"]: item["logprob"] for item in entries[0].get("top_logprobs", [])}
    if not {"yes", "no"} <= values.keys():
        raise ValueError(f"Missing yes/no logits; cannot invent scores: {values}")
    if not all(math.isfinite(values[k]) for k in ("yes", "no")):
        raise ValueError("Non-finite yes/no log probability")
    delta = values["no"] - values["yes"]
    relevance = math.exp(-delta) / (1 + math.exp(-delta)) if delta >= 0 else 1 / (1 + math.exp(delta))
    return relevance, values


def score(query, document, instruction, model=MODEL):
    prompt = PREFIX + f"<Instruct>: {instruction}\n<Query>: {query}\n<Document>: {document}" + SUFFIX
    response = request("/api/generate", {
        "model": model, "prompt": prompt, "raw": True, "stream": False,
        "logprobs": True, "top_logprobs": 20, "keep_alive": "10m",
        "options": {"temperature": 1, "top_k": 0, "top_p": 1, "seed": 42,
                    "num_predict": 1, "num_ctx": 8192, "repeat_penalty": 1},
    })
    relevance, values = probability(response.get("logprobs", []))
    return {"score": relevance, "answer": response["response"],
            "yes_logprob": values["yes"], "no_logprob": values["no"],
            "total_ms": response.get("total_duration", 0) / 1e6}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("query")
    parser.add_argument("documents", nargs="+")
    parser.add_argument("--instruction", default="根据查询，检索能够回答问题的相关段落")
    parser.add_argument("--model", default=MODEL)
    parser.add_argument("--backend", choices=["native", "ollama"], default="native")
    args = parser.parse_args()
    started = time.perf_counter()
    if args.backend == "native":
        report = request("/rerank", {"query": args.query, "documents": args.documents,
                         "instruction": args.instruction}, "http://127.0.0.1:3032")
    else:
        results = [dict(index=i, document=doc, **score(args.query, doc, args.instruction, args.model))
                   for i, doc in enumerate(args.documents)]
        results.sort(key=lambda r: r["score"], reverse=True)
        report = {"model": args.model, "results": results}
    report["elapsed_ms"] = (time.perf_counter() - started) * 1000
    print(json.dumps(report, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, URLError) as error:
        raise SystemExit(f"Reranking failed: {error}. See experiments/qwen-reranker/README.md")
