#!/usr/bin/env python3
"""Build top crates.io crates with sld as the Rust linker.

The default run fetches the current top 50 crates by total downloads from
crates.io, downloads each crate source tarball, and runs:

    cargo build

with RUSTFLAGS configured to use the local sld binary. If a crate fails with
sld, the same command is retried without sld in a separate target directory so
the report can distinguish sld-specific failures from crates that do not build
on the local host/toolchain.
"""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import json
import os
import platform
import shutil
import subprocess
import sys
import tarfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any


CRATES_API = "https://crates.io/api/v1"
USER_AGENT = "sld-top-crates-validation"


@dataclasses.dataclass(frozen=True)
class CrateSpec:
    name: str
    version: str
    downloads: int | None = None


@dataclasses.dataclass
class CommandResult:
    command: list[str]
    cwd: str
    returncode: int
    duration_secs: float
    log: str
    timed_out: bool = False


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Validate sld against top crates.io crates.",
    )
    parser.add_argument(
        "--limit",
        type=int,
        default=50,
        help="number of top-downloaded crates to fetch from crates.io (default: 50)",
    )
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
        help="directory for downloads, extracted crates, logs, and reports",
    )
    parser.add_argument(
        "--sld",
        type=Path,
        default=Path("target/release/sld"),
        help="sld binary to use as rustc's linker",
    )
    parser.add_argument(
        "--cargo",
        default=os.environ.get("CARGO", "cargo"),
        help="cargo executable to run",
    )
    parser.add_argument(
        "--rustup-toolchain",
        default=os.environ.get("RUSTUP_TOOLCHAIN", "stable"),
        help="RUSTUP_TOOLCHAIN value for cargo runs; use empty string to avoid setting it",
    )
    parser.add_argument(
        "--target",
        help="optional Rust target triple to pass to cargo",
    )
    parser.add_argument(
        "--jobs",
        type=int,
        help="optional -j value to pass to cargo",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=1800,
        help="timeout in seconds for each cargo invocation (default: 1800)",
    )
    parser.add_argument(
        "--test-no-run",
        action="store_true",
        help="run `cargo test --no-run` instead of `cargo build`",
    )
    parser.add_argument(
        "--all-targets",
        action="store_true",
        help="pass --all-targets to cargo",
    )
    parser.add_argument(
        "--all-features",
        action="store_true",
        help="pass --all-features to cargo",
    )
    parser.add_argument(
        "--locked",
        action="store_true",
        help="pass --locked to cargo",
    )
    parser.add_argument(
        "--stop-on-failure",
        action="store_true",
        help="stop after the first crate that does not validate",
    )
    parser.add_argument(
        "--no-baseline-on-failure",
        action="store_true",
        help="do not rerun failed crates with the system linker",
    )
    parser.add_argument(
        "--allow-baseline-failures",
        action="store_true",
        help=(
            "exit successfully when sld fails only on crates that also fail "
            "with the system linker"
        ),
    )
    parser.add_argument(
        "--keep-going-after-download-errors",
        action="store_true",
        help="continue if a crate source cannot be downloaded or extracted",
    )
    parser.add_argument(
        "--refresh",
        action="store_true",
        help="delete previously extracted copies before downloading/extracting",
    )
    parser.add_argument(
        "--print-crates",
        action="store_true",
        help="print the crate list before validation",
    )
    return parser.parse_args()


def now_iso() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat()


def fetch_json(url: str) -> Any:
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    for attempt in range(3):
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                return json.loads(response.read().decode("utf-8"))
        except urllib.error.URLError:
            if attempt == 2:
                raise
            time.sleep(2**attempt)
    raise RuntimeError(f"failed to fetch {url}")


def crate_version(crate: dict[str, Any]) -> str:
    for field in ("max_stable_version", "default_version", "max_version", "newest_version"):
        version = crate.get(field)
        if version:
            return str(version)
    raise RuntimeError(f"crate {crate.get('name') or crate.get('id')} has no usable version")


def fetch_top_crates(limit: int) -> list[CrateSpec]:
    if limit < 1:
        raise RuntimeError("--limit must be at least 1")
    url = f"{CRATES_API}/crates?sort=downloads&per_page={limit}"
    data = fetch_json(url)
    crates = data.get("crates", [])
    if len(crates) < limit:
        raise RuntimeError(f"crates.io returned {len(crates)} crates for limit {limit}")
    return [
        CrateSpec(
            name=str(crate["id"]),
            version=crate_version(crate),
            downloads=int(crate["downloads"]) if crate.get("downloads") is not None else None,
        )
        for crate in crates[:limit]
    ]


def fetch_named_crate(name: str) -> CrateSpec:
    quoted = urllib.parse.quote(name, safe="")
    data = fetch_json(f"{CRATES_API}/crates/{quoted}")
    crate = data["crate"]
    return CrateSpec(
        name=str(crate["id"]),
        version=crate_version(crate),
        downloads=int(crate["downloads"]) if crate.get("downloads") is not None else None,
    )


def explicit_crates(specs: list[str]) -> list[CrateSpec]:
    crates = []
    for spec in specs:
        if "@" in spec:
            name, version = spec.rsplit("@", 1)
            if not name or not version:
                raise RuntimeError(f"invalid crate spec {spec!r}; expected NAME[@VERSION]")
            crates.append(CrateSpec(name=name, version=version))
        else:
            crates.append(fetch_named_crate(spec))
    return crates


def request_download(url: str, destination: Path) -> None:
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    for attempt in range(3):
        try:
            with urllib.request.urlopen(request, timeout=120) as response:
                with destination.open("wb") as output:
                    shutil.copyfileobj(response, output)
            return
        except urllib.error.URLError:
            if attempt == 2:
                raise
            time.sleep(2**attempt)


def download_crate(crate: CrateSpec, dist_dir: Path) -> Path:
    dist_dir.mkdir(parents=True, exist_ok=True)
    archive = dist_dir / f"{crate.name}-{crate.version}.crate"
    if archive.exists():
        return archive

    quoted_name = urllib.parse.quote(crate.name, safe="")
    quoted_version = urllib.parse.quote(crate.version, safe="")
    url = f"{CRATES_API}/crates/{quoted_name}/{quoted_version}/download"
    partial = archive.with_suffix(archive.suffix + ".partial")
    request_download(url, partial)
    partial.replace(archive)
    return archive


def is_relative_to(path: Path, base: Path) -> bool:
    try:
        path.relative_to(base)
    except ValueError:
        return False
    return True


def safe_extract(archive: Path, extract_dir: Path) -> Path:
    extract_dir.mkdir(parents=True, exist_ok=True)
    base = extract_dir.resolve()
    with tarfile.open(archive, "r:gz") as crate_tar:
        members = crate_tar.getmembers()
        if not members:
            raise RuntimeError(f"{archive} is empty")
        roots = {member.name.split("/", 1)[0] for member in members if member.name}
        if len(roots) != 1:
            raise RuntimeError(f"{archive} does not contain exactly one root directory")
        root = base / roots.pop()

        for member in members:
            if member.issym() or member.islnk():
                raise RuntimeError(f"{archive} contains a link entry: {member.name}")
            target = (base / member.name).resolve()
            if not is_relative_to(target, base):
                raise RuntimeError(f"{archive} contains an unsafe path: {member.name}")

        crate_tar.extractall(base)

    if not (root / "Cargo.toml").exists():
        raise RuntimeError(f"{archive} did not extract to a crate root with Cargo.toml")
    return root


def prepare_crate(crate: CrateSpec, args: argparse.Namespace) -> Path:
    crate_dir = args.work_dir / "src" / f"{crate.name}-{crate.version}"
    if args.refresh and crate_dir.exists():
        shutil.rmtree(crate_dir)
    if (crate_dir / "Cargo.toml").exists():
        return crate_dir

    archive = download_crate(crate, args.work_dir / "dist")
    extracted = safe_extract(archive, args.work_dir / "src")
    if extracted.resolve() != crate_dir.resolve():
        if crate_dir.exists():
            shutil.rmtree(crate_dir)
        extracted.rename(crate_dir)
    isolate_from_parent_workspaces(crate_dir)
    return crate_dir


def isolate_from_parent_workspaces(crate_dir: Path) -> None:
    """Make the extracted crate a workspace root.

    The default work directory lives under sld's target directory. Without an
    explicit workspace root, Cargo walks up to sld's Cargo.toml and rejects the
    extracted crate as an unlisted workspace member.
    """
    manifest = crate_dir / "Cargo.toml"
    text = manifest.read_text(encoding="utf-8")
    if has_workspace_root(text):
        return
    with manifest.open("a", encoding="utf-8") as output:
        output.write("\n[workspace]\n")


def has_workspace_root(manifest: str) -> bool:
    return any(line.strip() == "[workspace]" for line in manifest.splitlines())


def cargo_command(args: argparse.Namespace) -> list[str]:
    if args.test_no_run:
        command = [args.cargo, "test", "--no-run"]
    else:
        command = [args.cargo, "build"]
    if args.all_targets:
        command.append("--all-targets")
    if args.target:
        command.extend(["--target", args.target])
    if args.jobs:
        command.extend(["-j", str(args.jobs)])
    if args.locked:
        command.append("--locked")
    if args.all_features:
        command.append("--all-features")
    return command


def sld_rustflags(args: argparse.Namespace) -> list[str]:
    sld = args.sld.resolve()
    if platform.system() == "Darwin":
        return ["-C", f"linker={sld}", "-C", "link-arg=-flavor", "-C", "link-arg=darwin"]

    linker_dir = args.work_dir / "sld-linker-driver"
    linker_dir.mkdir(parents=True, exist_ok=True)
    ld = linker_dir / "ld"
    if ld.exists() or ld.is_symlink():
        ld.unlink()
    ld.symlink_to(sld)
    return ["-C", "linker=cc", "-C", f"link-arg=-B{linker_dir}/"]


def append_rustflags(env: dict[str, str], flags: list[str]) -> None:
    existing = env.get("RUSTFLAGS", "").strip()
    joined = " ".join(flags)
    env["RUSTFLAGS"] = f"{existing} {joined}".strip()


def run_command(
    command: list[str],
    cwd: Path,
    env: dict[str, str],
    log: Path,
    timeout: int,
) -> CommandResult:
    start = time.monotonic()
    timed_out = False
    try:
        completed = subprocess.run(
            command,
            cwd=cwd,
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=timeout,
        )
        output = completed.stdout
        returncode = completed.returncode
    except subprocess.TimeoutExpired as error:
        timed_out = True
        output = error.stdout or ""
        if isinstance(output, bytes):
            output = output.decode("utf-8", errors="replace")
        output += f"\nTimed out after {timeout} seconds\n"
        returncode = 124

    duration = time.monotonic() - start
    log.parent.mkdir(parents=True, exist_ok=True)
    log.write_text(output, encoding="utf-8")
    return CommandResult(
        command=command,
        cwd=str(cwd),
        returncode=returncode,
        duration_secs=duration,
        log=str(log),
        timed_out=timed_out,
    )


def env_for_run(args: argparse.Namespace, target_dir_name: str, use_sld: bool) -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("CARGO_TERM_COLOR", "always")
    env.setdefault("CARGO_NET_RETRY", "3")
    if args.rustup_toolchain:
        env["RUSTUP_TOOLCHAIN"] = args.rustup_toolchain
    env["CARGO_HOME"] = str((args.work_dir / "cargo-home").resolve())
    env["CARGO_TARGET_DIR"] = str((args.work_dir / target_dir_name).resolve())
    if use_sld:
        append_rustflags(env, sld_rustflags(args))
    return env


def result_to_json(result: CommandResult | None) -> dict[str, Any] | None:
    if result is None:
        return None
    return {
        "command": result.command,
        "cwd": result.cwd,
        "returncode": result.returncode,
        "duration_secs": round(result.duration_secs, 3),
        "log": result.log,
        "timed_out": result.timed_out,
    }


def validate_crate(crate: CrateSpec, index: int, total: int, args: argparse.Namespace) -> dict[str, Any]:
    crate_dir = prepare_crate(crate, args)
    command = cargo_command(args)
    safe_name = crate.name.replace("/", "_")
    prefix = f"{index:02d}-{safe_name}-{crate.version}"
    print(f"[{index}/{total}] {crate.name} {crate.version}: sld", flush=True)
    sld = run_command(
        command,
        crate_dir,
        env_for_run(args, "cargo-target-sld", use_sld=True),
        args.work_dir / "logs" / f"{prefix}.sld.log",
        args.timeout,
    )
    baseline = None
    status = "passed"
    if sld.returncode != 0:
        status = "sld_failed"
        if not args.no_baseline_on_failure:
            print(f"[{index}/{total}] {crate.name} {crate.version}: baseline", flush=True)
            baseline = run_command(
                command,
                crate_dir,
                env_for_run(args, "cargo-target-baseline", use_sld=False),
                args.work_dir / "logs" / f"{prefix}.baseline.log",
                args.timeout,
            )
            if baseline.returncode != 0:
                status = "baseline_failed_too"

    return {
        "crate": crate.name,
        "version": crate.version,
        "downloads": crate.downloads,
        "status": status,
        "sld": result_to_json(sld),
        "baseline": result_to_json(baseline),
    }


def write_manifest(crates: list[CrateSpec], args: argparse.Namespace) -> None:
    manifest = {
        "fetched_at": now_iso(),
        "source": f"{CRATES_API}/crates?sort=downloads&per_page={args.limit}",
        "crates": [dataclasses.asdict(crate) for crate in crates],
    }
    (args.work_dir / "top-crates.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def write_summary(results: list[dict[str, Any]], args: argparse.Namespace) -> None:
    counts: dict[str, int] = {}
    for result in results:
        counts[result["status"]] = counts.get(result["status"], 0) + 1

    lines = [
        "# sld top crates validation",
        "",
        f"- generated: {now_iso()}",
        f"- sld: `{args.sld.resolve()}`",
        f"- command: `{' '.join(cargo_command(args))}`",
        f"- work dir: `{args.work_dir.resolve()}`",
        f"- counts: {', '.join(f'{key}={value}' for key, value in sorted(counts.items()))}",
        "",
        "| # | crate | version | status | sld seconds | baseline seconds | logs |",
        "|---:|---|---|---|---:|---:|---|",
    ]
    for index, result in enumerate(results, 1):
        sld = result.get("sld") or {}
        baseline = result.get("baseline") or {}
        sld_secs = sld.get("duration_secs", "")
        baseline_secs = baseline.get("duration_secs", "")
        logs = []
        if sld.get("log"):
            logs.append(f"[sld]({summary_link(args.work_dir, Path(sld['log']))})")
        if baseline.get("log"):
            logs.append(f"[baseline]({summary_link(args.work_dir, Path(baseline['log']))})")
        lines.append(
            "| {index} | `{crate}` | `{version}` | `{status}` | {sld_secs} | "
            "{baseline_secs} | {logs} |".format(
                index=index,
                crate=result["crate"],
                version=result["version"],
                status=result["status"],
                sld_secs=sld_secs,
                baseline_secs=baseline_secs,
                logs=", ".join(logs),
            )
        )
    (args.work_dir / "summary.md").write_text("\n".join(lines) + "\n", encoding="utf-8")


def summary_link(work_dir: Path, path: Path) -> str:
    try:
        return path.resolve().relative_to(work_dir.resolve()).as_posix()
    except ValueError:
        return path.as_posix()


def should_fail(results: list[dict[str, Any]], args: argparse.Namespace) -> bool:
    for result in results:
        status = result["status"]
        if status == "passed":
            continue
        if args.allow_baseline_failures and status == "baseline_failed_too":
            continue
        return True
    return False


def main() -> int:
    args = parse_args()
    args.work_dir.mkdir(parents=True, exist_ok=True)
    args.work_dir = args.work_dir.resolve()
    args.sld = args.sld.resolve()
    if not args.sld.exists():
        raise RuntimeError(f"sld binary does not exist: {args.sld}")

    crates = explicit_crates(args.crates) if args.crates else fetch_top_crates(args.limit)
    write_manifest(crates, args)
    if args.print_crates:
        for index, crate in enumerate(crates, 1):
            downloads = "" if crate.downloads is None else f" downloads={crate.downloads}"
            print(f"{index:02d}. {crate.name} {crate.version}{downloads}")

    results_path = args.work_dir / "results.jsonl"
    results_path.write_text("", encoding="utf-8")
    results = []
    for index, crate in enumerate(crates, 1):
        try:
            result = validate_crate(crate, index, len(crates), args)
        except (OSError, RuntimeError, tarfile.TarError, urllib.error.URLError) as error:
            result = {
                "crate": crate.name,
                "version": crate.version,
                "downloads": crate.downloads,
                "status": "setup_failed",
                "error": str(error),
                "sld": None,
                "baseline": None,
            }
            print(f"[{index}/{len(crates)}] {crate.name} {crate.version}: setup failed: {error}")
            if not args.keep_going_after_download_errors:
                results.append(result)
                with results_path.open("a", encoding="utf-8") as output:
                    output.write(json.dumps(result, sort_keys=True) + "\n")
                break

        results.append(result)
        with results_path.open("a", encoding="utf-8") as output:
            output.write(json.dumps(result, sort_keys=True) + "\n")
        write_summary(results, args)

        if result["status"] != "passed" and args.stop_on_failure:
            break

    write_summary(results, args)
    print(f"wrote {results_path}")
    print(f"wrote {args.work_dir / 'summary.md'}")
    return 1 if should_fail(results, args) else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        raise SystemExit(130)
    except Exception as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(2)
