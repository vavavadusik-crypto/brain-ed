#!/usr/bin/env python3
"""Run the supplied exam on a disposable HOME; emit counts/booleans only.

Executes the supplied exam.main() and measures the very same responses.
Raw answers and stderr are held only in memory. Use the SAME lexical-probe binary
for before/after to keep both noise definitions independent of the tested binary.
The caller must populate --home with a fresh copy of the same DB/settings snapshot
for each run. No models, databases, settings, or answer text belong in this repo.
"""
import argparse
import contextlib
import io
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
from unittest import mock


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--home", type=Path, required=True)
    parser.add_argument("--models", type=Path, required=True)
    parser.add_argument("--exam", type=Path, required=True)
    parser.add_argument("--probe", type=Path, required=True)
    parser.add_argument("--budget", type=int, default=12000)
    args = parser.parse_args()
    home = args.home.resolve()
    # Require a deliberate marker in a disposable copy, never the live HOME.
    if home == Path.home().resolve() or not (home / ".e31-disposable").is_file():
        parser.error("--home must be a marked disposable copy, not the current HOME")
    if not (home / ".brain/brain.sqlite").is_file():
        parser.error("copy the snapshot before running the exam")
    for relative in [".brain", ".brain/brain.sqlite", ".brain/settings.json",
                     ".brain/archive", ".brain/archive/sessions.sqlite"]:
        if not (home / relative).resolve().is_relative_to(home):
            parser.error("snapshot paths must stay inside the disposable HOME")
    spec = importlib.util.spec_from_file_location("exam", args.exam)
    exam = importlib.util.module_from_spec(spec)
    saved_argv = sys.argv
    sys.argv = [str(args.exam)]
    try:
        spec.loader.exec_module(exam)
    finally:
        sys.argv = saved_argv
    questions = [question for _, question, _, _ in exam.EXAM]
    probe = subprocess.run([str(args.probe.resolve()), *questions], capture_output=True, text=True)
    if probe.returncode:
        raise SystemExit("lexical probe failed (output suppressed)")
    normalization = json.loads(probe.stdout)
    env = {key: value for key, value in os.environ.items() if not key.startswith("BRAIN_")}
    env.update(HOME=str(home), BRAIN_MODELS_DIR=str(args.models.resolve()), BRAIN_OFFLINE="true")
    rows = []
    safe_stdout = sys.stdout
    run_command = subprocess.run
    exam.BRAIN = str(args.binary.resolve())
    exam.BUDGET = [str(args.budget)]

    def observed_run(command, **kwargs):
        # Execute the unchanged exam's command and observe that same response:
        # no second search, no change to its ordering, patterns or date checks.
        label, _, patterns, period = exam.EXAM[len(rows)]
        normalized = normalization[len(rows)]
        if not re.fullmatch(r"E\d+", label):
            raise SystemExit("unexpected exam label")
        start = time.monotonic()
        try:
            run = run_command(command, **kwargs, env=env, timeout=600)
            if run.returncode:
                raise ValueError("command failed")
            doc = json.loads(run.stdout)
        except (subprocess.TimeoutExpired, ValueError):
            row = {"label": label, "error": True}
            rows.append(row)
            print(json.dumps(row), file=safe_stdout, flush=True)
            return subprocess.CompletedProcess(command, 1, "", "")
        answers = exam.answers(doc)
        literal_noise = prefix_noise = 0
        for answer in answers:
            words = set(re.findall(r"\w+", answer["text"].lower()))
            literal_noise += not words.intersection(normalized["terms"])
            prefix_noise += not any(word.startswith(prefix) for word in words
                                    for prefix in normalized["prefixes"])
        signals = [r.get("signals", {}) for r in doc.get("results", [])[:5]]
        row = {
            "label": label, "hit": bool(exam.judge(answers, patterns, period, doc)),
            "top_count": len(answers), "literal_noise": literal_noise, "prefix_noise": prefix_noise,
            "exact_top5": sum(bool(s.get("exact")) for s in signals),
            "graph_top5": sum(s.get("graph_hops_away") is not None for s in signals),
            "vector_search": bool(doc.get("vector_search")),
            "elapsed_ms": round((time.monotonic() - start) * 1000),
        }
        rows.append(row)
        print(json.dumps(row), file=safe_stdout, flush=True)
        return run

    # Discard the exam's source/date strings, which are metadata from real data.
    # Only our fixed labels, counts and booleans are written to the caller.
    with mock.patch.object(exam.subprocess, "run", side_effect=observed_run):
        with contextlib.redirect_stdout(io.StringIO()):
            exam.main()
    print(json.dumps({"summary": True, "questions": len(rows),
                      "errors": sum(bool(r.get("error")) for r in rows),
                      "hits": sum(bool(r.get("hit")) for r in rows),
                      "top_count": sum(r.get("top_count", 0) for r in rows),
                      "literal_noise": sum(r.get("literal_noise", 0) for r in rows),
                      "prefix_noise": sum(r.get("prefix_noise", 0) for r in rows)}))
    return int(any(r.get("error") for r in rows))


if __name__ == "__main__":
    raise SystemExit(main())
