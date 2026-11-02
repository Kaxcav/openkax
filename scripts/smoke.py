#!/usr/bin/env python3
"""Smoke real do OpenKax (F0-14): tudo passa PELO GATEWAY local.

Uso: python3 scripts/smoke.py [--base http://127.0.0.1:8000] [--ledger DIR] [--out arquivo.md]

Passada 1 (texto): 1 request pinada por modelo habilitado (stream, max_tokens=64).
Passada 2 (tools): 3 primeiros degraus de kax/coder-smart e kax/coder-fast, tool `soma`.
Passada 3 (classes): 1 request por classe sem pin, stream=true e stream=false.
Providers pagos/crédito só na passada 1. Nunca imprime auth: só headers de limite
que o gateway expõe como x-openkax-upstream-*.
"""

import argparse
import glob
import json
import os
import sys
import time
import tomllib
import urllib.error
import urllib.request

PROMPT = "Responda apenas: ok"
TOOL_PROMPT = "quanto é 2+3? use a tool"
SOMA = {
    "type": "function",
    "function": {
        "name": "soma",
        "description": "Soma dois inteiros.",
        "parameters": {
            "type": "object",
            "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
            "required": ["a", "b"],
        },
    },
}


def post(base, body, headers, timeout=90):
    """POST /v1/chat/completions; devolve (status, headers, eventos SSE ou JSON, ttft_ms, total_ms)."""
    data = json.dumps(body).encode()
    req = urllib.request.Request(
        base + "/v1/chat/completions",
        data=data,
        headers={"content-type": "application/json", **headers},
        method="POST",
    )
    t0 = time.monotonic()
    ttft = None
    try:
        resp = urllib.request.urlopen(req, timeout=timeout)
    except urllib.error.HTTPError as e:
        raw = e.read().decode("utf-8", "replace")
        total = (time.monotonic() - t0) * 1000
        try:
            payload = json.loads(raw)
        except ValueError:
            payload = {"raw": raw[:300]}
        return e.code, dict(e.headers), payload, None, total
    hdrs = dict(resp.headers)
    if not body.get("stream"):
        payload = json.loads(resp.read().decode())
        total = (time.monotonic() - t0) * 1000
        return resp.status, hdrs, payload, None, total
    events = []
    buf = b""
    for line in resp:
        buf += line
        if line in (b"\n", b"\r\n"):
            for l in buf.decode("utf-8", "replace").splitlines():
                if l.startswith("data:"):
                    d = l[5:].strip()
                    if d == "[DONE]":
                        events.append("[DONE]")
                        continue
                    c = json.loads(d)
                    if ttft is None and any(
                        (ch.get("delta") or {}).get("content") or (ch.get("delta") or {}).get("tool_calls")
                        for ch in c.get("choices", [])
                    ):
                        ttft = (time.monotonic() - t0) * 1000
                    events.append(c)
            buf = b""
    total = (time.monotonic() - t0) * 1000
    return resp.status, hdrs, events, ttft, total


def with_retry(base, body, headers):
    """Respeita 1x o Retry-After do gateway (ex.: Mistral a 2 RPM)."""
    r = post(base, body, headers)
    if r[0] == 429:
        ra = {k.lower(): v for k, v in r[1].items()}.get("retry-after")
        if ra and ra.isdigit() and int(ra) <= 75:
            time.sleep(int(ra) + 1)
            r = post(base, body, headers)
    return r


def content_of(events):
    return "".join(
        (ch.get("delta") or {}).get("content") or ""
        for c in events
        if isinstance(c, dict)
        for ch in c.get("choices", [])
    )


def usage_of(events):
    for c in reversed(events):
        if isinstance(c, dict) and c.get("usage"):
            u = c["usage"]
            return f'{u.get("prompt_tokens")}/{u.get("completion_tokens")}'
    return "estimado"


def tool_calls_of(events):
    acc = {}
    for c in events:
        if not isinstance(c, dict):
            continue
        for ch in c.get("choices", []):
            for tc in (ch.get("delta") or {}).get("tool_calls") or []:
                e = acc.setdefault(tc.get("index", 0), {"name": "", "arguments": ""})
                f = tc.get("function") or {}
                e["name"] += f.get("name") or ""
                e["arguments"] += f.get("arguments") or ""
    return list(acc.values())


def tool_ok(calls):
    if not calls:
        return "não veio"
    c = calls[0]
    try:
        args = json.loads(c["arguments"] or "{}")
    except ValueError:
        return f"args inválidos: {c['arguments'][:40]!r}"
    if c["name"] == "soma" and args.get("a") == 2 and args.get("b") == 3:
        return "ok"
    return f"malformado: {c['name']} {args}"


def limit_headers(h):
    out = []
    for k, v in sorted(h.items(), key=lambda kv: kv[0].lower()):
        k = k.lower()
        if k.startswith("x-openkax-upstream-"):
            out.append(f"{k[len('x-openkax-upstream-'):]}={v}")
    return " ".join(out) or "—"


def last_ledger(ledger_dir):
    if not ledger_dir:
        return {}
    time.sleep(0.15)
    files = sorted(glob.glob(os.path.join(ledger_dir, "*.jsonl")))
    if not files:
        return {}
    with open(files[-1]) as f:
        lines = f.read().strip().splitlines()
    return json.loads(lines[-1]) if lines else {}


def err_text(payload):
    if isinstance(payload, dict):
        e = payload.get("error")
        if isinstance(e, dict):
            return str(e.get("message", e))[:140]
        return str(payload)[:140]
    return ""


def row(label, r, ledger, tools=None):
    status, hdrs, payload, ttft, total = r
    if status == 200 and isinstance(payload, list):
        txt = content_of(payload).strip().replace("\n", " ")[:24]
        fin = [ch.get("finish_reason") for c in payload if isinstance(c, dict) for ch in c.get("choices", []) if ch.get("finish_reason")]
        st = f"200 {fin[-1] if fin else '?'} «{txt}»"
        use = usage_of(payload)
    elif status == 200:
        msg = payload["choices"][0]["message"]
        st = f"200 «{(msg.get('content') or '').strip()[:24]}»"
        u = payload.get("usage")
        use = f'{u["prompt_tokens"]}/{u["completion_tokens"]}' if u else "estimado"
    else:
        st = f"{status} {err_text(payload)}"
        use = "—"
    reasoning = "sim" if ledger.get("reasoning") else ("não" if ledger else "?")
    return [
        label,
        st,
        f"{ttft:.0f}" if ttft else "—",
        f"{total:.0f}",
        use,
        reasoning,
        tools if tools is not None else "—",
        limit_headers(hdrs),
    ]


def table(rows):
    head = ["provider/model", "status", "ttft_ms", "total_ms", "usage in/out", "reasoning?", "tool_calls ok?", "x-ratelimit-*"]
    out = ["| " + " | ".join(head) + " |", "|" + "---|" * len(head)]
    for r in rows:
        out.append("| " + " | ".join(str(c).replace("|", "\\|") for c in r) + " |")
    return "\n".join(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://127.0.0.1:8000")
    ap.add_argument("--config", default="providers.toml")
    ap.add_argument("--ledger", default="")
    ap.add_argument("--out", default="")
    a = ap.parse_args()

    cfg = tomllib.load(open(a.config, "rb"))
    tiers = {p["id"]: p.get("tier", "free") for p in cfg["provider"]}
    quota = json.load(urllib.request.urlopen(a.base + "/quota"))
    report = []

    # ---------------- passada 1
    rows = []
    for p in quota["providers"]:
        for m in p["models"]:
            label = f'{p["id"]}/{m["id"]}'
            if p["state"]["state"] == "disabled" or m["state"]["state"] == "disabled":
                reason = p["state"].get("reason") or m["state"].get("reason")
                rows.append([label, f"skip ({reason})", "—", "—", "—", "—", "—", "—"])
                continue
            body = {
                "model": "kax/coder-smart",
                "stream": True,
                "stream_options": {"include_usage": True},
                "max_tokens": 64,
                "messages": [{"role": "user", "content": PROMPT}],
            }
            r = with_retry(a.base, body, {"x-openkax-pin": label})
            rows.append(row(label, r, last_ledger(a.ledger)))
            print("p1", label, rows[-1][1], file=sys.stderr)
    report.append("### Passada 1 — texto (pin, stream, max_tokens=64)\n\n" + table(rows))

    # ---------------- passada 2
    rows = []
    for cls in ("kax/coder-smart", "kax/coder-fast"):
        route = next(c for c in cfg["class"] if c["id"] == cls)["route"][:3]
        for step in route:
            label = f'{step["provider"]}/{step["model"]}'
            if tiers.get(step["provider"]) != "free":
                rows.append([f"{cls} {label}", "skip (tier pago/crédito)", "—", "—", "—", "—", "—", "—"])
                continue
            body = {
                "model": cls,
                "stream": True,
                "stream_options": {"include_usage": True},
                "max_tokens": 256,
                "tools": [SOMA],
                "tool_choice": "auto",
                "messages": [{"role": "user", "content": TOOL_PROMPT}],
            }
            r = with_retry(a.base, body, {"x-openkax-pin": label})
            tools = tool_ok(tool_calls_of(r[2])) if r[0] == 200 else "—"
            rows.append(row(f"{cls} {label}", r, last_ledger(a.ledger), tools))
            print("p2", label, rows[-1][1], tools, file=sys.stderr)
    report.append("### Passada 2 — tools (`soma`)\n\n" + table(rows))

    # ---------------- passada 3
    rows = []
    for c in cfg["class"]:
        for stream in (True, False):
            body = {
                "model": c["id"],
                "stream": stream,
                "max_tokens": 64,
                "messages": [{"role": "user", "content": PROMPT}],
            }
            if stream:
                body["stream_options"] = {"include_usage": True}
            # Pago/crédito só na passada 1: classes rodam com teto free.
            r = with_retry(a.base, body, {"x-openkax-max-tier": "free"})
            served = {k.lower(): v for k, v in r[1].items()}.get("x-openkax-provider", "—")
            led = last_ledger(a.ledger)
            label = f'{c["id"]} stream={str(stream).lower()} → {served}/{led.get("model", "?")} (fallbacks={led.get("fallbacks", "?")}, tier={led.get("tier", "?")})'
            rows.append(row(label, r, led))
            print("p3", label, rows[-1][1], file=sys.stderr)
    report.append("### Passada 3 — classes (sem pin)\n\n" + table(rows))

    final = json.load(urllib.request.urlopen(a.base + "/quota"))
    report.append("### /quota ao final\n\n```json\n" + json.dumps(
        {"served_by_tier": final.get("served_by_tier"), "ledger_dropped": final.get("ledger_dropped"),
         "request_faults": {f'{p["id"]}/{m["id"]}': m["request_faults"] for p in final["providers"] for m in p["models"] if m["request_faults"]},
         "nao_saudaveis": {f'{p["id"]}/{m["id"]}': [p["state"], m["state"]] for p in final["providers"] for m in p["models"]
                            if p["state"]["state"] != "healthy" or m["state"]["state"] != "healthy"}},
        ensure_ascii=False, indent=1) + "\n```")
    text = "\n\n".join(report)
    if a.out:
        open(a.out, "w").write(text + "\n")
    print(text)


if __name__ == "__main__":
    main()
