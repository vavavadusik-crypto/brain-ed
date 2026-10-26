"""Repeat the E4.6 regression with two CPU contenders on the same Linux CPU.

Build first with cargo test --workspace --no-run, then pass the brain unit-test
executable (debug/deps/brain-<hash>, not the CLI) as --binary. No retries: any
failed invocation or missing test ends the run with an error.
"""

import argparse
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import time


TEST = "ui::tests::cached_clusters_follow_scheduled_visibility_without_a_write"
LOAD = """
import hashlib
import signal
import sys
import time

started = time.process_time()
def finish(*_):
    print(f"load CPU seconds: {time.process_time() - started:.3f}", flush=True)
    sys.exit(0)
signal.signal(signal.SIGTERM, finish)
data = b"E4.6 synthetic CPU load" * 1024
while True:
    hashlib.sha256(data).digest()
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=200)
    parser.add_argument("--cpu", type=int)
    args = parser.parse_args()
    if args.repeats < 1:
        parser.error("--repeats must be positive")
    binary = args.binary.resolve(strict=True)
    cpu = min(os.sched_getaffinity(0)) if args.cpu is None else args.cpu
    os.sched_setaffinity(0, {cpu})
    print(f"binary: {binary}", flush=True)
    with binary.open("rb") as executable:
        digest = hashlib.file_digest(executable, "sha256").hexdigest()
    print(f"sha256: {digest}", flush=True)
    print(f"CPU affinity: {sorted(os.sched_getaffinity(0))}; nice: {os.getpriority(os.PRIO_PROCESS, 0)}", flush=True)
    workers = []
    started = time.monotonic()
    try:
        for _ in range(2):
            workers.append(subprocess.Popen([sys.executable, "-c", LOAD]))
        print(f"CPU load workers: {[worker.pid for worker in workers]}", flush=True)
        for iteration in range(1, args.repeats + 1):
            if any(worker.poll() is not None for worker in workers):
                raise RuntimeError("CPU load worker exited early")
            result = subprocess.run(
                [str(binary), "--exact", TEST, "--nocapture"],
                capture_output=True, text=True, timeout=60,
            )
            if result.returncode or "1 passed; 0 failed" not in result.stdout:
                raise RuntimeError(f"iteration {iteration}: {result.stdout}\n{result.stderr}")
            if iteration % 25 == 0 or iteration == args.repeats:
                print(f"PASS {iteration}/{args.repeats}", flush=True)
        print(f"PASS: {args.repeats} consecutive invocations; elapsed {time.monotonic() - started:.3f}s", flush=True)
    finally:
        for worker in workers:
            worker.terminate()
        for worker in workers:
            worker.wait(timeout=10)


if __name__ == "__main__":
    main()
