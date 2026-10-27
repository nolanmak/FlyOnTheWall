#!/usr/bin/env python3
"""Run pinned local comparisons, or explicitly opt into declared cloud adapters."""
import argparse
import json
import os
from pathlib import Path
import sys
import tempfile

import runner


def write_report(path, report):
    """Publish a private complete JSON file atomically; never replace an old run."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".speech-eval-", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as output:
            json.dump(report, output, indent=2, sort_keys=True, allow_nan=False)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.link(temporary, path)
    finally:
        os.unlink(temporary)


def preflight_output(path):
    """Check private temporary writes and atomic publication before inference."""
    try:
        parent = Path(path).parent
        parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix=".speech-eval-check-", dir=parent) as owned:
            source = Path(owned) / "source"
            with source.open("xb") as output:
                output.write(b"report preflight")
                output.flush()
                os.fsync(output.fileno())
            os.link(source, Path(owned) / "linked")
    except OSError as error:
        raise ValueError("report directory cannot write and atomically publish a report") from error


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    hashing = commands.add_parser("hash-asset", help="fingerprint an explicit local file or model bundle")
    hashing.add_argument("path", type=Path)
    for name, help_text in (("run", "local-only; never resolve a cloud key"),
                            ("run-cloud", "explicitly permit declared Deepgram adapters and approved fixture uploads")):
        command = commands.add_parser(name, help=help_text)
        command.add_argument("--corpus", type=Path, required=True)
        command.add_argument("--candidates", type=Path, required=True)
        command.add_argument("--output", type=Path, required=True, help="new report path; existing files are preserved")
        command.add_argument("--repetitions", type=int, default=1)
        command.add_argument("--max-seconds", type=float, default=3600,
                             help="execution-phase wall budget after artifact verification")
    args = parser.parse_args(argv)
    try:
        if args.command == "hash-asset":
            print(json.dumps({"sha256": runner.asset_digest(args.path)}))
            return 0
        if args.output.exists() or args.output.is_symlink():
            raise ValueError("report path already exists; choose a new output path")
        preflight_output(args.output)
        report = runner.run(args.corpus, args.candidates, repetitions=args.repetitions,
                            allow_cloud=args.command == "run-cloud", max_total_seconds=args.max_seconds)
        write_report(args.output, report)
        print(json.dumps({"paired_execution_complete": report["paired_execution_complete"],
                          "successful_trials": sum(r["status"] == "ok" for r in report["rows"]),
                          "total_trials": len(report["rows"])}))
        return 0 if report["paired_execution_complete"] else 1
    except (OSError, ValueError) as error:
        print("Speech evaluation refused: " + str(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
