#!/usr/bin/env python3
"""Summarize loop-unroll stats and final IR/assembly size for one target."""

import argparse
import csv
from collections import Counter
from pathlib import Path


def parse_fields(line):
    fields = {}
    for field in line.rstrip().split("\t")[1:]:
        key, value = field.split("=", 1)
        fields[key] = value
    return fields


def parse_stats(path):
    summary = {}
    rejects = Counter()
    trips = Counter()
    events = {}
    if not path.exists():
        return summary, rejects, trips, events
    for line in path.read_text(errors="replace").splitlines():
        if line.startswith("loop_unroll_event\t"):
            fields = parse_fields(line)
            events[(fields["function"], fields["header"])] = fields
        elif line.startswith("loop_unroll_summary\t"):
            summary = parse_fields(line)
        elif line.startswith("loop_unroll_reject\t"):
            fields = parse_fields(line)
            rejects[fields["reason"]] += int(fields["count"])
        elif line.startswith("loop_unroll_trip\t"):
            fields = parse_fields(line)
            trips[int(fields["trip"])] += int(fields["count"])
    return summary, rejects, trips, events


def count_ir(path):
    if not path.exists():
        return None
    return sum(
        1
        for line in path.read_text(errors="replace").splitlines()
        if line.startswith("    ") and line.strip()
    )


def count_asm(path):
    if not path.exists():
        return None
    count = 0
    for line in path.read_text(errors="replace").splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith(".") or stripped.endswith(":"):
            continue
        count += 1
    return count


def cases(root):
    return sorted(path.relative_to(root) for path in root.rglob("*.s"))


def delta(on, off):
    return None if on is None or off is None else on - off


def write_tsv(path, header, rows):
    with path.open("w", newline="") as file:
        writer = csv.writer(file, delimiter="\t")
        writer.writerow(header)
        writer.writerows(rows)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--on", type=Path, required=True)
    parser.add_argument("--off", type=Path, required=True)
    parser.add_argument("--dry-run", type=Path, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)

    case_rows = []
    event_rows = []
    rejects = Counter()
    trips = Counter()
    totals = Counter()
    for asm_rel in cases(args.on):
        stem = asm_rel.with_suffix("")
        on_asm = count_asm(args.on / asm_rel)
        off_asm = count_asm(args.off / asm_rel)
        on_ir = count_ir((args.on / stem).with_suffix(".raana"))
        off_ir = count_ir((args.off / stem).with_suffix(".raana"))
        summary, case_rejects, case_trips, case_events = parse_stats(
            (args.dry_run / stem).with_suffix(".compile.stderr")
        )
        rejects.update(case_rejects)
        trips.update(case_trips)
        for fields in case_events.values():
            event_rows.append(
                [
                    args.target,
                    str(stem),
                    fields["function"],
                    fields["header"],
                    fields["outcome"],
                    fields["reason"],
                    fields["trip"],
                    fields["header_size"],
                    fields["body_size"],
                    fields["projected"],
                ]
            )
        for key in [
            "unique_loops",
            "shape_candidates",
            "exact_trip_candidates",
            "would_apply",
        ]:
            totals[key] += int(summary.get(key, 0))
        case_rows.append(
            [
                args.target,
                str(stem),
                summary.get("unique_loops", "0"),
                summary.get("shape_candidates", "0"),
                summary.get("exact_trip_candidates", "0"),
                summary.get("would_apply", "0"),
                off_ir,
                on_ir,
                delta(on_ir, off_ir),
                off_asm,
                on_asm,
                delta(on_asm, off_asm),
            ]
        )

    write_tsv(
        args.output / "cases.tsv",
        [
            "target",
            "case",
            "unique_loops",
            "shape_candidates",
            "exact_trip_candidates",
            "would_apply",
            "ir_off",
            "ir_on",
            "ir_delta",
            "asm_off",
            "asm_on",
            "asm_delta",
        ],
        case_rows,
    )
    write_tsv(
        args.output / "events.tsv",
        [
            "target",
            "case",
            "function",
            "header",
            "outcome",
            "reason",
            "trip",
            "header_size",
            "body_size",
            "projected",
        ],
        event_rows,
    )
    write_tsv(
        args.output / "rejects.tsv",
        ["reason", "count"],
        sorted(rejects.items()),
    )
    write_tsv(
        args.output / "trip_counts.tsv",
        ["trip", "count"],
        sorted(trips.items()),
    )

    ir_delta = sum(row[8] or 0 for row in case_rows)
    asm_delta = sum(row[11] or 0 for row in case_rows)
    summary_rows = [
        ["target", args.target],
        ["cases", len(case_rows)],
        ["unique_loops", totals["unique_loops"]],
        ["shape_candidates", totals["shape_candidates"]],
        ["exact_trip_candidates", totals["exact_trip_candidates"]],
        ["would_apply", totals["would_apply"]],
        ["hit_rate_vs_shape", f"{totals['would_apply'] / totals['shape_candidates']:.6f}" if totals["shape_candidates"] else "0"],
        ["final_ir_delta", ir_delta],
        ["final_asm_delta", asm_delta],
    ]
    write_tsv(args.output / "summary.tsv", ["metric", "value"], summary_rows)
    for metric, value in summary_rows:
        print(f"{metric}: {value}")
    print(f"report: {args.output}")


if __name__ == "__main__":
    main()
