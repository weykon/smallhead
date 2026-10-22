"""Hands-on local Laya decisions. Python standard library only; no LLM API key."""
import argparse
import json
import math
import os
import statistics
import time
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener

from service import validate_request

ROOT = Path(__file__).resolve().parent


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class Client:
    def __init__(self, url):
        parsed = urlsplit(url)
        if (parsed.scheme != "http" or parsed.hostname != "127.0.0.1"
                or not parsed.port or parsed.username or parsed.password
                or parsed.query or parsed.fragment or parsed.path not in ("", "/")):
            raise ValueError("URL must be http://127.0.0.1:<port>")
        self.url = url.rstrip("/")
        self.opener = build_opener(ProxyHandler({}), NoRedirect())

    def request(self, path, payload=None):
        data = None if payload is None else json.dumps(payload, ensure_ascii=False, allow_nan=False).encode()
        request = Request(self.url + path, data, {"Content-Type": "application/json"})
        try:
            with self.opener.open(request, timeout=30) as response:
                raw = response.read(65537)
                if len(raw) > 65536:
                    raise ValueError("Response exceeds 64 KiB")
                return json.loads(raw)
        except HTTPError as error:
            raise RuntimeError(f"HTTP {error.code}: {error.read(4096).decode(errors='replace')}") from error
        except URLError as error:
            raise RuntimeError(f"Laya unavailable at {self.url}: {error.reason}. See experiments/laya/README.md") from error

    def decide(self, case):
        # Gold labels, fixture outputs and case IDs never enter the model request.
        payload = {key: case[key] for key in ("goal", "state", "candidates")}
        choices = validate_request(payload)["operation"]["criteria"]
        started = time.perf_counter()
        result = self.request("/decide", payload)
        if result.get("operation") not in choices:
            raise ValueError("Service returned an unknown operation")
        probs = result.get("probabilities", {})
        if (set(probs) != set(choices) or any(isinstance(v, bool) or not isinstance(v, (int, float))
                or not math.isfinite(v) or not 0 <= v <= 1 for v in probs.values())
                or abs(sum(probs.values()) - 1) > .01):
            raise ValueError("Invalid probability distribution")
        return result, round((time.perf_counter() - started) * 1000, 2)


def load_cases(path):
    cases = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    if not cases:
        raise ValueError("Empty case file")
    for case in cases:
        choices = validate_request({k: case[k] for k in ("goal", "state", "candidates")})
        if case["expected"] not in choices["operation"]["criteria"]:
            raise ValueError("Expected label not in choices")
    if len({c["id"] for c in cases}) != len(cases):
        raise ValueError("Duplicate case IDs")
    return cases


def show(case, result, elapsed):
    selected = result["operation"]
    print(f"\n[{case['id']}] {case['state'].get('query', case['state'])}")
    print(f"selected={selected} expected={case.get('expected', 'unlabelled')} HTTP={elapsed}ms inference={result.get('inference_ms')}ms")
    print("probabilities=" + json.dumps(result["probabilities"], ensure_ascii=False))
    # Selection retrieves a fixed demo payload, never executes arbitrary operations.
    if selected == "defer":
        print("execution=not_started: 模型选择 defer，需要补充信息或交给主 Agent。")
    elif selected in case.get("outputs", {}):
        print("fixture result=" + json.dumps(case["outputs"][selected], ensure_ascii=False))
    else:
        print(f"execution=not_started: 已选中 {selected}；此例只演示路由，没有执行器或预设结果。")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default=os.environ.get("SMALLHEAD_LAYA_URL", "http://127.0.0.1:3031"))
    parser.add_argument("--cases", type=Path, default=ROOT / "cases.jsonl")
    parser.add_argument("--output", type=Path, default=Path(".smallhead/laya/report.json"))
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("health")
    sub.add_parser("list")
    run = sub.add_parser("run")
    run.add_argument("case_id")
    run.add_argument("--query", help="Override the query; no gold label is assumed")
    bench = sub.add_parser("bench")
    bench.add_argument("--reverse", action="store_true", help="Also test reversed candidate order")
    args = parser.parse_args()
    if args.command == "list":
        for case in load_cases(args.cases):
            print(case["id"], case["state"]["query"])
        return
    client = Client(args.url)
    health = client.request("/health")
    if args.command == "health":
        print(json.dumps(health, ensure_ascii=False, indent=2))
        return
    cases = load_cases(args.cases)
    if args.command == "run":
        case = next((c for c in cases if c["id"] == args.case_id), None)
        if case is None:
            raise ValueError("Unknown case ID; use list")
        if args.query:
            case["state"]["query"] = args.query
            case.pop("expected")
            case.pop("outputs", None)
        result, elapsed = client.decide(case)
        show(case, result, elapsed)
        return
    results = []
    for case in cases:
        for reverse in ([False, True] if args.reverse else [False]):
            item = dict(case, candidates=list(reversed(case["candidates"])) if reverse else case["candidates"])
            row = {"id": case["id"], "reversed": reverse, "expected": case["expected"]}
            try:
                result, elapsed = client.decide(item)
                row.update(prediction=result, elapsed_ms=elapsed, correct=result["operation"] == case["expected"])
                show(item, result, elapsed)
            except (RuntimeError, ValueError) as error:
                row.update(error=str(error), correct=False)
                print(f"ERROR {case['id']}: {error}")
            results.append(row)
    timings = [r["elapsed_ms"] for r in results if "elapsed_ms" in r]
    report = {"health": health, "synthetic": True, "total": len(results),
              "correct": sum(r["correct"] for r in results),
              "errors": sum("error" in r for r in results),
              "median_http_ms": statistics.median(timings) if timings else None,
              "results": results}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(f"\n{report['correct']}/{report['total']} correct; errors={report['errors']}; report={args.output}")
    if report["errors"]:
        raise SystemExit(1)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, ValueError, OSError, KeyError) as error:
        raise SystemExit(str(error))
