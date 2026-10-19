#!/usr/bin/env python3
"""The `find` regression battery: hand-written questions with known answers, asked of a running gardend over MCP.

    TOKEN=<owner or client token, or empty for a public gardend> \
    scripts/find-battery.py --base https://cow-tools.sophia-labs.com \
        --set catalogue:uruguay-catalogue --set cows:uruguay-cows [--set wikidata:<graph>] [--k 10] [--out report.json]

Question sets live in src-tauri/tests/fixtures/find-battery/<set>.json (from the 2026-10-14 spike, written by agents that
never saw the design): catalogue (80, Spanish + English, golds are W2's dataset IRIs), cows (40, cow-tools' IRIs),
wikidata (40, a Uruguay + Argentina Wikidata slice). A hit counts when its iri, or the iri of the part it rolled up
(`via`), is a gold answer. The floors are the spike's e5-small numbers less a margin; the exit code says whether every
set held its floor. Only the standard library: runs anywhere Python 3.9+ does.
"""
import argparse, json, os, sys, time, urllib.request
from pathlib import Path

FLOORS = {"catalogue": 0.85, "cows": 0.60, "wikidata": 0.75}  # hit@10
HERE = Path(__file__).resolve().parent.parent / "src-tauri" / "tests" / "fixtures" / "find-battery"


def call(base, token, name, arguments):
    headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    body = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": arguments}}
    request = urllib.request.Request(base.rstrip("/") + "/mcp", data=json.dumps(body).encode(), headers=headers)
    for attempt in range(5):
        try:
            raw = urllib.request.urlopen(request, timeout=120).read().decode()
            break
        except urllib.error.HTTPError as error:
            if error.code == 429 and attempt < 4:
                time.sleep(float(error.headers.get("Retry-After") or 2))
                continue
            raise
    if "data:" in raw[:40]:
        raw = "\n".join(line[5:].strip() for line in raw.splitlines() if line.startswith("data:"))
    reply = json.loads(raw)
    result = reply.get("result") or {}
    text = "".join(part.get("text", "") for part in result.get("content", []) if part.get("type") == "text")
    if result.get("isError") or "error" in reply:
        raise RuntimeError(text or json.dumps(reply.get("error")))
    return json.loads(text)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", default="http://127.0.0.1:7674")
    parser.add_argument("--set", action="append", required=True, help="<set>:<graph id>")
    parser.add_argument("--k", type=int, default=10)
    parser.add_argument("--pause", type=float, default=0.6, help="seconds between calls (public rate limits)")
    parser.add_argument("--out")
    args = parser.parse_args()
    token = os.environ.get("TOKEN", "")
    report, ok = {"base": args.base, "k": args.k, "sets": {}}, True
    for spec in args.set:
        name, graph = spec.split(":", 1)
        questions = json.loads((HERE / f"{name}.json").read_text())
        rows, hit1, hit10, mrr, ms = [], 0, 0, 0.0, []
        by_lang = {}
        for q in questions:
            started = time.time()
            answer = call(args.base, token, "find", {"graphId": graph, "query": q["q"], "k": args.k})
            ms.append((time.time() - started) * 1000)
            gold = set(q["gold"])
            rank = next((i for i, hit in enumerate(answer["hits"])
                         if hit["iri"] in gold or (hit.get("via") or {}).get("iri") in gold), None)
            hit1 += rank == 0
            hit10 += rank is not None and rank < 10
            mrr += 1 / (rank + 1) if rank is not None and rank < 10 else 0
            lang = by_lang.setdefault(q.get("lang", "?"), [0, 0])
            lang[0] += 1
            lang[1] += rank is not None and rank < 10
            rows.append({"id": q["id"], "q": q["q"], "rank": rank, "top3": [h["label"] for h in answer["hits"][:3]]})
            time.sleep(args.pause)
        n = len(questions)
        ms.sort()
        summary = {"graph": graph, "n": n, "hit@1": hit1 / n, "hit@10": hit10 / n, "mrr@10": mrr / n,
                   "byLang": {l: v[1] / v[0] for l, v in by_lang.items()},
                   "ms_p50": ms[len(ms) // 2], "ms_p95": ms[int(len(ms) * 0.95) - 1],
                   "floor": FLOORS.get(name), "questions": rows}
        summary["held"] = summary["floor"] is None or summary["hit@10"] >= summary["floor"]
        ok &= summary["held"]
        report["sets"][name] = summary
        print(f"{name:10} {graph:20} n={n:3} hit@1 {summary['hit@1']:.2f} hit@10 {summary['hit@10']:.2f} "
              f"mrr {summary['mrr@10']:.2f} {json.dumps({l: round(v, 2) for l, v in summary['byLang'].items()})} "
              f"p50 {summary['ms_p50']:.0f} ms p95 {summary['ms_p95']:.0f} ms floor {summary['floor']} "
              f"{'held' if summary['held'] else 'BELOW'}")
    if args.out:
        Path(args.out).write_text(json.dumps(report, indent=1, ensure_ascii=False))
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
