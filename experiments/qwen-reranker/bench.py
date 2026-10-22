"""Run the same synthetic Laya fixtures with native Qwen reranking."""
import json
from pathlib import Path
from rerank import request

root = Path(__file__).resolve().parent
cases = [json.loads(line) for line in (root.parent / "laya/cases.jsonl").read_text().splitlines() if line.strip()]
results = []
for case in cases:
    candidates = case["candidates"] + [{"operation": "defer", "description": "需要更多证据或推理；这些操作均不适合。"}]
    result = request("/rerank", {
        "query": json.dumps(case["state"], ensure_ascii=False),
        "instruction": case["goal"],
        "documents": [c["operation"] + ": " + c["description"] for c in candidates],
    }, "http://127.0.0.1:3032")
    selected = candidates[result["results"][0]["index"]]["operation"]
    row = {"id": case["id"], "selected": selected, "expected": case["expected"],
           "correct": selected == case["expected"], "prediction": result}
    results.append(row)
    print(case["id"], selected, "expected=" + case["expected"], flush=True)
report = {"synthetic": True, "correct": sum(r["correct"] for r in results), "total": len(results),
          "note": "Same cases, different scoring protocol; reranker defer is an explicit document, not a calibrated rejection threshold.",
          "results": results}
destination = root / "baseline-2026-10-06.json"
destination.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
print(f"{report['correct']}/{report['total']}; {destination}")
