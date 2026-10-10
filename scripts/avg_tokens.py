#!/usr/bin/env python3
"""
Per-model request statistics over the last N hours from boom_request_log:
request volume, avg input/output tokens, avg TTFT, and prefix hit rate.

Zero external dependencies — reads general_settings.database_url from the
gateway config (env-var references resolved) and shells out to psql.

Usage:
    python avg_tokens.py <config.yaml> [hours]

    hours: time window in hours (default: 24). Supports fractions like 0.5.

All metrics count 2xx responses only. Hit rate follows the dashboard
convention: per-request cached_tokens / input_tokens * 100, averaged over
requests where both are recorded.
"""

import os
import re
import subprocess
import sys


def resolve_env(value: str) -> str:
    """Resolve os.environ/VAR or ${VAR} references."""
    m = re.match(r"^os\..environ/(.+)$", value) or re.match(r"^\$\{(.+)\}$", value)
    if m:
        return os.environ[m.group(1)]
    return value


def parse_yaml_db_url(path: str) -> str:
    """Extract general_settings.database_url from YAML without PyYAML."""
    with open(path) as f:
        in_gs = False
        for line in f:
            stripped = line.rstrip()
            if stripped == "general_settings:":
                in_gs = True
                continue
            if in_gs:
                if stripped and not stripped[0].isspace():
                    break
                m = re.match(r"\s+database_url:\s*[\"']?(.+?)[\"']?\s*$", stripped)
                if m:
                    return m.group(1)
    return ""


def main():
    if len(sys.argv) not in (2, 3):
        print(f"Usage: {sys.argv[0]} <config.yaml> [hours]")
        sys.exit(1)

    config_path = sys.argv[1]
    hours_str = sys.argv[2] if len(sys.argv) == 3 else "24"

    try:
        hours = float(hours_str)
    except ValueError:
        print(f"Error: hours must be a number, got '{hours_str}'")
        sys.exit(1)
    if hours <= 0:
        print(f"Error: hours must be positive, got {hours}")
        sys.exit(1)
    hours_label = f"{hours:g}"

    try:
        db_url = parse_yaml_db_url(config_path)
    except OSError as e:
        print(f"Error: cannot read config '{config_path}': {e}")
        sys.exit(1)
    if not db_url:
        print("Error: general_settings.database_url not found in config")
        sys.exit(1)
    db_url = resolve_env(db_url)

    # hours_label is a validated float re-rendered via :g, so only digits
    # and a dot can reach the SQL string.
    sql = f"""
SELECT
    COALESCE(model_name, model) AS model,
    COUNT(*) AS requests,
    COUNT(input_tokens) AS input_count,
    COUNT(output_tokens) AS output_count,
    COUNT(ttft_ms) AS ttft_count,
    COUNT(cached_tokens) AS cached_count,
    AVG(input_tokens) AS avg_input,
    AVG(output_tokens) AS avg_output,
    AVG(ttft_ms) AS avg_ttft,
    AVG(CASE WHEN cached_tokens IS NOT NULL
              AND COALESCE(input_tokens, 0) > 0
             THEN cached_tokens::double precision
                  / input_tokens * 100.0 END) AS hit_rate
FROM boom_request_log
WHERE created_at >= NOW() - INTERVAL '{hours_label} hours'
  AND status_code >= 200 AND status_code < 300
GROUP BY COALESCE(model_name, model)
ORDER BY requests DESC
"""

    try:
        result = subprocess.run(
            ["psql", db_url, "-t", "-A", "-F", "|", "-c", sql],
            capture_output=True, text=True,
        )
    except FileNotFoundError:
        print("Error: psql not found in PATH")
        sys.exit(1)

    if result.returncode != 0:
        print(f"psql error: {result.stderr.strip()}")
        sys.exit(1)

    rows = []
    for line in result.stdout.strip().splitlines():
        line = line.strip()
        if not line:
            continue
        parts = line.split("|")
        if len(parts) != 10:
            continue
        (model, total, in_cnt, out_cnt, ttft_cnt, cached_cnt,
         avg_in, avg_out, avg_ttft, hit_rate) = parts
        rows.append({
            "model": model,
            "requests": int(total),
            "in_count": int(in_cnt),
            "out_count": int(out_cnt),
            "ttft_count": int(ttft_cnt),
            "cached_count": int(cached_cnt),
            "avg_in": float(avg_in) if avg_in else None,
            "avg_out": float(avg_out) if avg_out else None,
            "avg_ttft": float(avg_ttft) if avg_ttft else None,
            "hit_rate": float(hit_rate) if hit_rate else None,
        })

    if not rows:
        print(f"No successful (2xx) requests found in the last {hours_label} hours")
        return

    def num(v, count):
        return f"{v:.1f}" if v is not None and count > 0 else "-"

    def pct(v):
        # Truncate instead of round so 99.96% never displays as 100%.
        return f"{int(v * 10) / 10:.1f}" if v is not None else "-"

    model_w = max(len("model"), max(len(r["model"]) for r in rows))
    header = (f"{'model':<{model_w}}  {'requests':>8}  {'avg_in':>9}  "
              f"{'avg_out':>9}  {'avg_ttft_ms':>11}  {'hit_rate%':>9}")
    print(f"Model statistics — last {hours_label}h, 2xx responses only")
    print()
    print(header)
    print("-" * len(header))
    for r in rows:
        print(f"{r['model']:<{model_w}}  {r['requests']:>8}  "
              f"{num(r['avg_in'], r['in_count']):>9}  "
              f"{num(r['avg_out'], r['out_count']):>9}  "
              f"{num(r['avg_ttft'], r['ttft_count']):>11}  "
              f"{pct(r['hit_rate']):>9}")


if __name__ == "__main__":
    main()
