"""Wall-clock cost of `loch hook` per call, for any number of loch binaries.

    python rust/bench/hook.py [--runs N] <loch> [<loch> ...]

Each binary runs against its own scratch repository with the event Claude Code
sends around a Bash call. Three cases, since they cost different amounts:
`pre-tool`, `post-tool` when the call committed nothing (nearly every call),
and `post-tool` when it made a commit and a note gets written. Reports the
median and p90 in milliseconds. Nothing from a real transcript is used.
"""

import argparse
import json
import os
import statistics
import subprocess
import sys
import tempfile
import time

SESSION = "00000000-0000-4000-8000-000000000000"


def git(cwd: str, *args: str) -> None:
    subprocess.run(["git", "-C", cwd, *args], check=True, capture_output=True)


def scratch_repo(root: str) -> str:
    repo = os.path.join(root, "repo")
    os.makedirs(repo)
    git(repo, "init", "-q", "-b", "main")
    git(repo, "commit", "-q", "--allow-empty", "-m", "initial")
    return repo


def run(loch: str, name: str, event: dict, env: dict) -> float:
    started = time.perf_counter()
    subprocess.run(
        [loch, "hook", name],
        input=json.dumps(event).encode(),
        env=env,
        check=True,
        capture_output=True,
    )
    return (time.perf_counter() - started) * 1000


def bench(loch: str, runs: int) -> dict[str, list[float]]:
    times: dict[str, list[float]] = {"pre-tool": [], "post-tool": [], "post-commit": []}
    with tempfile.TemporaryDirectory() as root:
        repo = scratch_repo(root)
        env = {**os.environ, "HOME": root, "GIT_CONFIG_GLOBAL": os.devnull}
        env.pop("CLAUDE_CONFIG_DIR", None)
        for i in range(runs):
            event = {
                "session_id": SESSION,
                "cwd": repo,
                "tool_name": "Bash",
                "tool_input": {"command": f"echo {i}"},
                "tool_use_id": f"toolu_{i:08d}",
            }
            times["pre-tool"].append(run(loch, "pre-tool", event, env))
            if i % 2:
                git(repo, "commit", "-q", "--allow-empty", "-m", f"change {i}")
                times["post-commit"].append(run(loch, "post-tool", event, env))
            else:
                times["post-tool"].append(run(loch, "post-tool", event, env))
    return times


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--runs", type=int, default=40)
    parser.add_argument("loch", nargs="+")
    args = parser.parse_args()
    print(f"{'binary':<48} {'case':<12} {'median':>8} {'p90':>8}")
    for loch in args.loch:
        for case, samples in bench(loch, args.runs).items():
            samples.sort()
            p90 = samples[int(len(samples) * 0.9) - 1]
            print(
                f"{loch[-48:]:<48} {case:<12} "
                f"{statistics.median(samples):>7.1f}ms {p90:>7.1f}ms"
            )
    sys.stdout.flush()


if __name__ == "__main__":
    main()
