"""Running commands: argv lists only (never a shell), with a hard timeout."""

import contextlib
import os
import signal
import subprocess
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path


@dataclass(frozen=True)
class Completed:
    argv: tuple[str, ...]
    returncode: int
    stdout: str
    stderr: str
    timed_out: bool

    @property
    def ok(self) -> bool:
        return self.returncode == 0 and not self.timed_out

    @property
    def output(self) -> str:
        """stdout then stderr, for logs and pattern checks."""
        return self.stdout + self.stderr


@dataclass(frozen=True)
class RunAs:
    """Runs commands as another (unprivileged) user through sudo.

    Plain variables travel in the argv through env(1), where a reviewer can
    see them in the log; secrets travel only in sudo's environment and are
    named in --preserve-env, so they never appear in a process's argv
    (/proc/<pid>/cmdline is readable by every user).
    """

    user: str
    env: Mapping[str, str] = field(default_factory=dict)
    secrets: Mapping[str, str] = field(default_factory=dict)

    def wrap(self, argv: Sequence[str], extra: Mapping[str, str] | None = None) -> list[str]:
        prefix = ["sudo", "-n", "-u", self.user, "-H"]
        if self.secrets:
            prefix.append("--preserve-env=" + ",".join(sorted(self.secrets)))
        variables = {**self.env, **(extra or {})}
        assignments = [f"{name}={value}" for name, value in sorted(variables.items())]
        return [*prefix, "--", "/usr/bin/env", *assignments, *argv]

    def sudo_env(self) -> dict[str, str]:
        """sudo's own environment: a PATH to find it, and the secrets."""
        return {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), **self.secrets}

    def kill_argv(self) -> list[str]:
        """Stops every process of the user; sudo runs as root, so the
        caller cannot signal it or its children itself."""
        return ["sudo", "-n", "pkill", "-KILL", "-u", self.user]


# The signature verification and the agent step call commands through, so
# tests can record or fake them.
Runner = Callable[..., Completed]


def run(
    argv: Sequence[str],
    cwd: Path,
    timeout: float,
    env: Mapping[str, str] | None = None,
    run_as: RunAs | None = None,
    merge_output: bool = False,
) -> Completed:
    """Runs `argv` in `cwd`, stopping it and everything it started on timeout.

    `env` holds variables to set on top of the baseline: this process's
    environment, or with `run_as` only `run_as.env` (sudo resets the rest).
    `merge_output` interleaves stderr into stdout in the order written.
    """
    if run_as is None:
        full_argv = list(argv)
        popen_env = {**os.environ, **(env or {})}
    else:
        full_argv = run_as.wrap(argv, env)
        popen_env = run_as.sudo_env()
    # A new session, so a timed-out `cargo test` takes its test binaries
    # (and anything they spawned) down with it.
    process = subprocess.Popen(
        full_argv,
        cwd=cwd,
        env=popen_env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT if merge_output else subprocess.PIPE,
        text=True,
        errors="replace",
        start_new_session=True,
    )
    timed_out = False
    try:
        stdout, stderr = process.communicate(timeout=max(timeout, 1))
    except subprocess.TimeoutExpired:
        timed_out = True
        _stop(process, run_as)
        stdout, stderr = process.communicate()
    return Completed(tuple(argv), process.returncode, stdout, stderr or "", timed_out)


def _stop(process: subprocess.Popen[str], run_as: RunAs | None) -> None:
    if run_as is not None:
        subprocess.run(
            run_as.kill_argv(), stdin=subprocess.DEVNULL, capture_output=True, check=False
        )
    with contextlib.suppress(ProcessLookupError, PermissionError):
        os.killpg(process.pid, signal.SIGKILL)


def tail(text: str, limit: int) -> str:
    """The last `limit` characters, where test failures and errors are."""
    if len(text) <= limit:
        return text
    return "…" + text[-(limit - 1) :]
