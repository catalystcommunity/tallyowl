"""Running a tool, once, in one place.

This is a process-execution helper, not a workflow framework. It standardizes
the working directory, the argument form, and secret-safe display, and leaves
orchestration to the caller. See docs/CI-CD.md section 3.

Two rules it exists to hold:

- **an argument array, never a shell string.** `shell=False` means no quoting
  rule, no word splitting, and no injection through a path with a space in it;
- **no secret reaches the terminal.** A value passed as sensitive is replaced
  before the command is printed.
"""

from __future__ import annotations

import os
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Mapping, Sequence

REPOSITORY_ROOT = Path(__file__).resolve().parents[2]

GREEN = "\033[0;32m"
RED = "\033[0;31m"
DIM = "\033[2m"
RESET = "\033[0m"


def _colour(text: str, code: str) -> str:
    if not sys.stdout.isatty() or os.environ.get("NO_COLOR"):
        return text
    return f"{code}{text}{RESET}"


def say(message: str) -> None:
    print(_colour(f"==> {message}", GREEN), flush=True)


def warn(message: str) -> None:
    print(_colour(f"==> {message}", RED), file=sys.stderr, flush=True)


class ToolFailed(Exception):
    """A tool exited non-zero. The message is what a person needs to do next."""

    def __init__(self, message: str, exit_code: int = 1) -> None:
        super().__init__(message)
        self.exit_code = exit_code


class ToolMissing(ToolFailed):
    """A required program is not installed."""


@dataclass
class Result:
    exit_code: int
    stdout: str
    stderr: str

    @property
    def ok(self) -> bool:
        return self.exit_code == 0


def run(
    command: Sequence[str],
    *,
    cwd: Path | None = None,
    env: Mapping[str, str] | None = None,
    capture: bool = False,
    check: bool = True,
    sensitive: Iterable[str] = (),
    quiet: bool = False,
) -> Result:
    """Run one program with an argument array."""
    argv = [str(part) for part in command]
    shown = " ".join(argv)
    for secret in sensitive:
        if secret:
            shown = shown.replace(secret, "[a secret]")
    if not quiet:
        print(_colour(f"  $ {shown}", DIM), flush=True)

    full_env = os.environ.copy()
    if env:
        full_env.update(env)

    try:
        completed = subprocess.run(  # noqa: S603 - an array, never a shell string
            argv,
            cwd=str(cwd or REPOSITORY_ROOT),
            env=full_env,
            shell=False,
            text=True,
            capture_output=capture,
        )
    except FileNotFoundError as error:
        raise ToolMissing(
            f"`{argv[0]}` is not installed, and this step needs it. {error}"
        ) from error

    result = Result(
        exit_code=completed.returncode,
        stdout=completed.stdout or "",
        stderr=completed.stderr or "",
    )
    if check and not result.ok:
        if capture:
            sys.stdout.write(result.stdout)
            sys.stderr.write(result.stderr)
        raise ToolFailed(f"`{argv[0]}` failed.", exit_code=result.exit_code)
    return result


def which(program: str) -> str | None:
    from shutil import which as _which

    return _which(program)


def require(program: str, how_to_install: str) -> str:
    """Find a program, or say plainly how to get it."""
    found = which(program)
    if not found:
        raise ToolMissing(f"`{program}` is not installed. {how_to_install}")
    return found
