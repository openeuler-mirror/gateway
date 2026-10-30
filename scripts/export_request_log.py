#!/usr/bin/env python3
"""
Export raw request logs (boom_request_log) to CSV, read-only and chunked.

Zero external dependencies — reads general_settings.database_url from the
gateway config (env-var references resolved; falls back to $BOOM_DATABASE_URL
for env-bootstrap deployments) and shells out to psql.

Usage:
    python export_request_log.py <models> <hours> [out.csv] [--config config.yaml]

    python export_request_log.py "GLM-5.3,DeepSeek" 24 logs.csv
    python export_request_log.py all 24                    # CSV to stdout
    python export_request_log.py all 24 - > logs.csv       # same, explicit

Arguments:
    models  Comma-separated model names, matched against BOTH the request-time
            model column (model) and the resolved deployment model column
            (model_name). "all" (case-insensitive) exports every model.
            NOTE: "*" is NOT a wildcard here — in this gateway it is a real
            model_name (the fallback route); pass "all" to export everything.
    hours   Time window back from now; fractions allowed (0.5 = 30 min).
    out     Output CSV path. Omit (or "-") to stream CSV to stdout.

Output: all boom_request_log columns (SELECT *), newest first.

Large exports are safe for a live DB: keyset pagination on created_at (never
OFFSET), each chunk is an independent short query with LIMIT and
statement_timeout, the session is forced read-only, and the scan rides the
idx_request_log_created index. Rows sharing one created_at (one batch INSERT
= up to ~2000 rows with the same NOW() default) are handled by re-fetching
the boundary tie group per chunk and de-duplicating by id client-side.
"""

import argparse
import csv
import io
import math
import os
import re
import subprocess
import sys
import time

STATEMENT_TIMEOUT_MS = 120_000
INTER_CHUNK_PAUSE_S = 0.2
ESCALATION_LIMIT_CAP = 8_000_000


def resolve_env(value: str) -> str:
    """Resolve os.environ/VAR or ${VAR} references."""
    m = re.match(r"^os\..environ/(.+)$", value) or re.match(r"^\$\{(.+)\}$", value)
    if m:
        return os.environ[m.group(1)]
    return value


def parse_yaml_db_url(path: str) -> str:
    """Extract general_settings.database_url from YAML without PyYAML."""
    try:
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
    except OSError:
        return ""
    return ""


def get_db_url(config_path: str) -> str:
    url = parse_yaml_db_url(config_path)
    if url:
        return resolve_env(url)
    url = os.environ.get("BOOM_DATABASE_URL", "")
    if url:
        return url
    sys.exit(f"Error: no database_url in '{config_path}' and $BOOM_DATABASE_URL "
             f"is unset. Point --config at the gateway config.yaml, or export "
             f"BOOM_DATABASE_URL.")


def sql_literal(s: str) -> str:
    """Single-quoted SQL literal; doubling quotes is sufficient under
    standard_conforming_strings (PG/openGauss default). Backslash and NUL are
    rejected outright so the literal stays safe even with SCS off."""
    if "\\" in s or "\x00" in s:
        sys.exit(f"Error: value contains a backslash or NUL, refusing to embed: {s!r}")
    return "'" + s.replace("'", "''") + "'"


def build_models_clause(models):
    lits = ", ".join(sql_literal(m) for m in models)
    return f"(model IN ({lits}) OR model_name IN ({lits}))"


def build_query(models_clause: str, hours_label: str, frontier: str, limit: int) -> str:
    conds = [f"created_at >= NOW() - INTERVAL '{hours_label} hours'"]
    if frontier is not None:
        # Frontier text came from this DB's own COPY output (digits and
        # :.-+ only), never from user input.
        if not re.fullmatch(r"[\d:.+\- ]+", frontier):
            sys.exit(f"Error: unexpected created_at value from DB: {frontier!r}")
        conds.append(f"created_at <= {sql_literal(frontier)}")
    if models_clause:
        conds.append(models_clause)
    return ("SELECT * FROM boom_request_log WHERE " + " AND ".join(conds)
            + " ORDER BY created_at DESC LIMIT " + str(limit))


def start_copy(db_url: str, query: str, header: bool) -> subprocess.Popen:
    """Run `COPY (query) TO STDOUT` as a streamed CSV; psql writes to stdout."""
    fmt = "WITH (FORMAT csv, HEADER true)" if header else "WITH (FORMAT csv)"
    env = os.environ.copy()
    # read-only session: even a bug in this script cannot write anything;
    # statement_timeout caps a runaway chunk; UTC keeps output deterministic.
    env["PGOPTIONS"] = ("-c timezone=UTC "
                        f"-c statement_timeout={STATEMENT_TIMEOUT_MS} "
                        "-c default_transaction_read_only=on")
    env["PGCONNECT_TIMEOUT"] = "10"
    return subprocess.Popen(
        ["psql", db_url, "-X", "-q", "-v", "ON_ERROR_STOP=1",
         "-c", f"COPY ({query}) TO STDOUT {fmt}"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
    )


def query_rows(db_url: str, query: str):
    """Small helper result fetch (COPY CSV) used for the no-rows hint."""
    proc = start_copy(db_url, query, header=False)
    with io.TextIOWrapper(proc.stdout, encoding="utf-8", errors="replace", newline="") as f:
        rows = [row for row in csv.reader(f) if row]
    err = proc.stderr.read()
    if proc.wait() != 0:
        sys.exit(f"Error: psql failed: {err.strip()}")
    return rows


def suggest_models_in_window(db_url: str, hours_label: str):
    q = ("SELECT COALESCE(model_name, model), COUNT(*) "
         f"FROM boom_request_log WHERE created_at >= NOW() - INTERVAL '{hours_label} hours' "
         "GROUP BY 1 ORDER BY 2 DESC LIMIT 20")
    try:
        rows = query_rows(db_url, q)
    except SystemExit:
        return ""
    if not rows:
        return ""
    names = ", ".join(f"{r[0]} ({r[1]} rows)" for r in rows)
    return f"\nModels actually present in this window: {names}"


def main():
    ap = argparse.ArgumentParser(
        description="Export boom_request_log rows to CSV (read-only, chunked).",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__.split("Usage:")[1] if "Usage:" in __doc__ else None,
    )
    ap.add_argument("models",
                    help='comma-separated model names, or "all" for every model')
    ap.add_argument("hours", type=float,
                    help="time window back from now; fractions allowed (e.g. 0.5)")
    ap.add_argument("out", nargs="?",
                    help="output CSV path; omit or '-' to stream CSV to stdout")
    ap.add_argument("--config", default="config.yaml",
                    help="gateway config to read database_url from (default: config.yaml)")
    ap.add_argument("--chunk-size", type=int, default=50_000,
                    help="rows per query (default: 50000; minimum 1000). If a "
                         "created_at tie group (~2000 rows from one batch "
                         "INSERT) exceeds the chunk, the loop auto-escalates "
                         "the limit by 8x until it makes progress")
    args = ap.parse_args()

    if not math.isfinite(args.hours) or args.hours <= 0:
        ap.error("hours must be a positive number")
    hours_label = f"{args.hours:g}"
    if args.chunk_size < 1_000:
        ap.error("chunk-size must be >= 1000")

    requested = [m.strip() for m in args.models.split(",") if m.strip()]
    all_mode = any(m.lower() == "all" for m in requested)
    if all_mode:
        if len(requested) > 1:
            print('Note: "all" present — exporting every model, other names ignored.',
                  file=sys.stderr)
        models_clause = ""
    else:
        for m in requested:
            if m == "*":
                print('Note: "*" is a real model_name in this gateway (fallback '
                      'route), not a wildcard. Exporting logs for model "*" only; '
                      'pass "all" for everything.', file=sys.stderr)
        models_clause = build_models_clause(requested)

    db_url = get_db_url(args.config)

    if args.out and args.out != "-":
        out = open(args.out, "w", newline="", encoding="utf-8")
    else:
        out = sys.stdout
    writer = csv.writer(out, lineterminator="\n")

    # Keyset cursor: created_at of the last emitted row. Rows tying on
    # created_at (batch INSERTs) are re-fetched each chunk and skipped by id.
    frontier = None
    seen_tie_ids = set()
    col_ts = col_id = None
    total = 0
    chunk_no = 0
    limit = args.chunk_size
    started = time.time()

    try:
        while True:
            chunk_no += 1
            query = build_query(models_clause, hours_label, frontier, limit)
            proc = start_copy(db_url, query, header=(chunk_no == 1))
            fetched = emitted = 0
            with io.TextIOWrapper(proc.stdout, encoding="utf-8",
                                  errors="replace", newline="") as stream:
                for row in csv.reader(stream):
                    if col_ts is None:
                        # First record of the first chunk: the CSV header row.
                        try:
                            col_ts = row.index("created_at")
                            col_id = row.index("id")
                        except ValueError:
                            sys.exit("Error: boom_request_log lacks a created_at "
                                     "or id column — cannot paginate safely.")
                        writer.writerow(row)
                        continue
                    fetched += 1
                    ts, rid = row[col_ts], row[col_id]
                    if ts == frontier and rid in seen_tie_ids:
                        continue
                    writer.writerow(row)
                    emitted += 1
                    total += 1
                    if ts == frontier:
                        seen_tie_ids.add(rid)
                    else:
                        frontier = ts
                        seen_tie_ids = {rid}
            err = proc.stderr.read()
            if proc.wait() != 0:
                sys.exit(f"Error: psql failed on chunk {chunk_no}: {err.strip()}")
            print(f"chunk {chunk_no}: fetched {fetched}, new {emitted}, "
                  f"total {total}", file=sys.stderr)

            if fetched < limit:
                break  # window exhausted
            if emitted == 0:
                # Pathological tie group larger than the chunk: refetch wider.
                if limit >= ESCALATION_LIMIT_CAP:
                    sys.exit(f"Error: no progress even at limit {limit}; "
                             f"a single created_at ties more rows than "
                             f"{ESCALATION_LIMIT_CAP}. Aborting.")
                limit *= 8
                print(f"chunk {chunk_no}: tie group exceeds chunk, retrying "
                      f"with limit {limit}", file=sys.stderr)
                continue
            time.sleep(INTER_CHUNK_PAUSE_S)
    except KeyboardInterrupt:
        target = args.out if args.out and args.out != "-" else "stdout"
        print(f"\nAborted — partial CSV ({total} rows) at {target}", file=sys.stderr)
        sys.exit(130)
    finally:
        if out is not sys.stdout:
            out.close()

    elapsed = time.time() - started
    target = args.out if args.out and args.out != "-" else "stdout"
    print(f"Done: {total} rows in {chunk_no} chunk(s), {elapsed:.1f}s, "
          f"window = last {hours_label}h, output = {target}", file=sys.stderr)
    if total == 0:
        print(f"No matching rows in the last {hours_label} hours.", file=sys.stderr)
        if models_clause:
            print(suggest_models_in_window(db_url, hours_label), file=sys.stderr)


if __name__ == "__main__":
    main()
