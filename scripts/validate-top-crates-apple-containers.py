#!/usr/bin/env python3
"""Run top-crates validation on macOS and Linux via Apple's container tool."""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any


DEFAULT_IMAGE = "rust:1.95.0-bookworm"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Validate top crates on native macOS and Linux via Apple containers.",
    )
    parser.add_argument("--limit", type=int, default=50)
    parser.add_argument(
        "--crate",
        dest="crates",
        action="append",
        default=[],
        metavar="NAME[@VERSION]",
        help="validate an explicit crate instead of the top list; repeatable",
    )
    parser.add_argument(
        "--work-dir",
        type=Path,
        default=Path("target/top-crates-validation"),
        help="root directory for per-platform reports",
    )
    parser.add_argument(
        "--sld",
        type=Path,
        default=Path("target/release/sld"),
        help="native macOS sld binary",
    )
    parser.add_argument(
        "--rustup-toolchain",
        default="stable",
        help="RUSTUP_TOOLCHAIN value for cargo validation runs; use empty string to avoid setting it",
    )
    parser.add_argument(
        "--container",
        default="container",
        help="Apple container CLI executable",
    )
    parser.add_argument(
        "--linux-image",
        default=DEFAULT_IMAGE,
        help=f"Linux image used by Apple containers (default: {DEFAULT_IMAGE})",
    )
    parser.add_argument(
        "--linux-dns",
        default="1.1.1.1",
        help="DNS server for Linux containers; pass an empty string to use the runtime default",
    )
    parser.add_argument(
        "--linux-memory",
        default="4g",
        help="memory limit for Linux containers; pass an empty string to use the runtime default",
    )
    parser.add_argument(
        "--linux-cpus",
        type=int,
        help="CPU count for Linux containers; omitted uses the runtime default",
    )
    parser.add_argument("--timeout", type=int, default=1800)
    parser.add_argument("--jobs", type=int)
    parser.add_argument("--test-no-run", action="store_true")
    parser.add_argument("--all-targets", action="store_true")
    parser.add_argument("--all-features", action="store_true")
    parser.add_argument("--locked", action="store_true")
    parser.add_argument("--stop-on-failure", action="store_true")
    parser.add_argument("--allow-baseline-failures", action="store_true")
    parser.add_argument("--refresh", action="store_true")
    parser.add_argument(
        "--platform",
        choices=["macos", "linux"],
        action="append",
        help="platform lane to run; repeatable; defaults to both",
    )
    parser.add_argument(
        "--skip-linux-sld-build",
        action="store_true",
        help="reuse the Linux sld binary already built in the Linux work dir",
    )
    parser.add_argument(
        "--linux-sld-profile",
        choices=["debug", "release"],
        default="debug",
        help="profile used to build sld inside the Linux container (default: debug)",
    )
    parser.add_argument(
        "--print-crates",
        action="store_true",
        help="print the crate list in each lane",
    )
    return parser.parse_args()


def run(command: list[str], cwd: Path) -> int:
    print("+ " + " ".join(command), flush=True)
    return subprocess.run(command, cwd=cwd).returncode


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def validation_args(args: argparse.Namespace) -> list[str]:
    command = ["--limit", str(args.limit), "--timeout", str(args.timeout)]
    if args.rustup_toolchain:
        command.extend(["--rustup-toolchain", args.rustup_toolchain])
    for crate in args.crates:
        command.extend(["--crate", crate])
    if args.jobs:
        command.extend(["--jobs", str(args.jobs)])
    if args.test_no_run:
        command.append("--test-no-run")
    if args.all_targets:
        command.append("--all-targets")
    if args.all_features:
        command.append("--all-features")
    if args.locked:
        command.append("--locked")
    if args.stop_on_failure:
        command.append("--stop-on-failure")
    if args.allow_baseline_failures:
        command.append("--allow-baseline-failures")
    if args.refresh:
        command.append("--refresh")
    if args.print_crates:
        command.append("--print-crates")
    return command


def run_macos(args: argparse.Namespace, root: Path) -> int:
    work_dir = args.work_dir.resolve() / "macos"
    command = [
        sys.executable,
        str(root / "scripts" / "validate-top-crates.py"),
        "--work-dir",
        str(work_dir),
        "--sld",
        str(args.sld.resolve()),
        *validation_args(args),
    ]
    return run(command, root)


def linux_shell_command(args: argparse.Namespace, root: Path) -> str:
    linux_base = container_path(root, args.work_dir)
    linux_work = f"{linux_base}/linux"
    linux_sld_target = f"{linux_base}/linux-sld-target"
    profile_dir = "release" if args.linux_sld_profile == "release" else "debug"
    linux_sld = f"{linux_sld_target}/{profile_dir}/sld"
    pieces = [
        "set -euo pipefail",
        "export PATH=/usr/local/rustup/toolchains/1.95.0-aarch64-unknown-linux-gnu/bin:/usr/local/cargo/bin:$PATH",
    ]
    if not args.skip_linux_sld_build:
        build = ["cargo", "build", "--bin", "sld", "--target-dir", linux_sld_target]
        if args.linux_sld_profile == "release":
            build.insert(2, "--release")
        pieces.append(shell_join(build))
    validate = [
        "python3",
        "/work/scripts/validate-top-crates.py",
        "--work-dir",
        linux_work,
        "--sld",
        linux_sld,
        *validation_args(args),
    ]
    pieces.append(shell_join(validate))
    return " && ".join(pieces)


def container_path(root: Path, host_path: Path) -> str:
    try:
        relative = host_path.resolve().relative_to(root.resolve())
    except ValueError as error:
        raise RuntimeError(
            f"Linux validation work-dir must be inside the mounted repo: {host_path}"
        ) from error
    return "/work/" + relative.as_posix()


def shell_join(parts: list[str]) -> str:
    return " ".join(shell_quote(part) for part in parts)


def shell_quote(value: str) -> str:
    if all(char.isalnum() or char in "@%_+=:,./-" for char in value):
        return value
    return "'" + value.replace("'", "'\"'\"'") + "'"


def run_linux(args: argparse.Namespace, root: Path) -> int:
    if not shutil.which(args.container):
        print(f"error: Apple container CLI not found: {args.container}", file=sys.stderr)
        return 2
    command = [
        args.container,
        "run",
        "--rm",
        "--volume",
        f"{root}:/work",
        "--workdir",
        "/work",
    ]
    if args.linux_memory:
        command.extend(["--memory", args.linux_memory])
    if args.linux_cpus:
        command.extend(["--cpus", str(args.linux_cpus)])
    if args.linux_dns:
        command.extend(["--dns", args.linux_dns])
    command.extend([args.linux_image, "bash", "-lc", linux_shell_command(args, root)])
    return run(command, root)


def write_combined_summary(args: argparse.Namespace) -> None:
    root = args.work_dir.resolve()
    summary: dict[str, Any] = {}
    for platform in ("macos", "linux"):
        results = root / platform / "results.jsonl"
        if not results.exists():
            continue
        rows = [json.loads(line) for line in results.read_text().splitlines() if line.strip()]
        counts: dict[str, int] = {}
        for row in rows:
            counts[row["status"]] = counts.get(row["status"], 0) + 1
        summary[platform] = {"count": len(rows), "statuses": counts}
    (root / "combined-summary.json").write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def main() -> int:
    args = parse_args()
    root = repo_root()
    args.work_dir = args.work_dir.resolve()
    args.work_dir.mkdir(parents=True, exist_ok=True)
    platforms = args.platform or ["macos", "linux"]
    status = 0
    if "macos" in platforms:
        status = run_macos(args, root) or status
    if "linux" in platforms:
        status = run_linux(args, root) or status
    write_combined_summary(args)
    print(f"wrote {args.work_dir / 'combined-summary.json'}")
    return status


if __name__ == "__main__":
    raise SystemExit(main())
