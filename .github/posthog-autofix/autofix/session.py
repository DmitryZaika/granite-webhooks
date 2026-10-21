"""Verification's command runner: one time budget, one log."""

import re
import shlex
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path

from . import proc

_ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
# Each command's output in the log keeps its end, where the failures are.
_LOG_OUTPUT_LIMIT = 30_000
_MIN_SECONDS = 15


class OutOfTime(RuntimeError):
    pass


def strip_ansi(text: str) -> str:
    return _ANSI.sub("", text)


# Set for every check: plain output, no colours, CI mode.
QUIET_ENV = {"NO_COLOR": "1", "FORCE_COLOR": "0", "CARGO_TERM_COLOR": "never", "CI": "true"}


@dataclass
class Session:
    runner: proc.Runner
    deadline: float
    # The agent's user, in CI: checks execute agent-written code.
    run_as: proc.RunAs | None = None
    lines: list[str] = field(default_factory=list)
    clock: Callable[[], float] = time.monotonic

    @classmethod
    def start(
        cls, runner: proc.Runner, budget_seconds: float, run_as: proc.RunAs | None = None
    ) -> "Session":
        return cls(runner=runner, deadline=time.monotonic() + budget_seconds, run_as=run_as)

    def log(self, line: str) -> None:
        self.lines.append(line)

    def execute(
        self,
        label: str,
        argv: Sequence[str],
        cwd: Path,
        cap_seconds: float,
        env_extra: Mapping[str, str] | None = None,
    ) -> proc.Completed:
        remaining = self.deadline - self.clock()
        if remaining < _MIN_SECONDS:
            raise OutOfTime(f"verification ran out of time before: {label}")
        self.log(f"$ {shlex.join(argv)}    # {label}, in {cwd.name}")
        completed = self.runner(
            argv,
            cwd=cwd,
            timeout=min(cap_seconds, remaining),
            env={**QUIET_ENV, **(env_extra or {})},
            run_as=self.run_as,
        )
        completed = proc.Completed(
            completed.argv,
            completed.returncode,
            strip_ansi(completed.stdout),
            strip_ansi(completed.stderr),
            completed.timed_out,
        )
        status = "timed out" if completed.timed_out else f"exit {completed.returncode}"
        self.log(proc.tail(completed.output, _LOG_OUTPUT_LIMIT))
        self.log(f"-> {status}\n")
        return completed

    def write_log(self, path: Path) -> None:
        path.write_text("\n".join(self.lines) + "\n")
