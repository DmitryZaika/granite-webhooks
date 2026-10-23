"""Which Bash commands the guard blocks.

A second line behind the permission rules (which already deny anything not
allowlisted): it catches the dangerous cases even if a rule is too wide.
It errs towards blocking: a command it cannot read with certainty is
refused. It is not a sandbox; `cargo test` runs code the agent wrote.
"""

import os
import re
import shlex
from dataclasses import dataclass, field
from pathlib import PurePosixPath

from .paths import is_secret_file, is_write_protected

SECRET_VARIABLES = ("CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY", "POSTHOG_AUTOFIX_MCP_KEY")
_PUNCTUATION = "();<>|&\n"
_ASSIGNMENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*=")
_DURATION = re.compile(r"\d+(\.\d+)?[smhd]?")

_BLOCKED_COMMANDS: dict[str, str] = {}
for _name in (
    "curl",
    "wget",
    "nc",
    "ncat",
    "netcat",
    "ssh",
    "scp",
    "sftp",
    "rsync",
    "telnet",
    "socat",
    "ftp",
):
    _BLOCKED_COMMANDS[_name] = "network tools are blocked"
for _name in ("env", "printenv", "export", "declare", "typeset", "set", "compgen"):
    _BLOCKED_COMMANDS[_name] = "reading the environment is blocked"
for _name in (
    "sudo",
    "su",
    "doas",
    "eval",
    "exec",
    "source",
    ".",
    "bash",
    "sh",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "xargs",
):
    _BLOCKED_COMMANDS[_name] = "shells, eval and sudo are blocked"
_BLOCKED_COMMANDS.update(
    {
        "gh": "the GitHub CLI is blocked",
        "aws": "the AWS CLI is blocked",
        "cargo-lambda": "cargo lambda (deploys) is blocked",
    }
)
# Wrappers that run the rest of the line as a command.
_TRANSPARENT = ("command", "nohup", "time", "nice", "timeout", "stdbuf")
_GIT_BLOCKED = ("push", "remote", "config", "fetch", "pull", "clone", "ls-remote", "submodule")
_PACKAGE_RUNNERS = ("make", "bun", "bunx", "npm", "npx", "yarn", "pnpm")


class Blocked(Exception):
    """Raised with the one-line reason a command is refused."""


@dataclass
class SimpleCommand:
    words: list[str] = field(default_factory=list)
    writes: list[str] = field(default_factory=list)
    reads: list[str] = field(default_factory=list)


def check(command: str, cwd: str, workspace: str) -> str | None:
    """The reason to block `command` (run in `cwd`), or None to allow it."""
    try:
        _check(command, cwd, workspace)
    except Blocked as blocked:
        return str(blocked)
    return None


def _check(command: str, cwd: str, workspace: str) -> None:
    for marker in ("$(", "`", "<(", ">(", "${"):
        if marker in command:
            raise Blocked("command and process substitution are blocked")
    tokens = tokenize(command)
    _check_tokens(tokens)
    current: str | None = cwd
    for simple in split_commands(tokens):
        current = _check_simple(simple, current, workspace)


def tokenize(command: str) -> list[str]:
    # Bash drops backslash-newline before reading words; newlines are kept
    # as separators (shlex would treat them as plain whitespace).
    lexer = shlex.shlex(command.replace("\\\n", ""), posix=True, punctuation_chars=_PUNCTUATION)
    lexer.whitespace = " \t\r"
    lexer.whitespace_split = True
    lexer.commenters = ""
    try:
        return list(lexer)
    except ValueError:
        raise Blocked("the command cannot be parsed (unbalanced quotes?)") from None


def _check_tokens(tokens: list[str]) -> None:
    for token in tokens:
        if any(name in token for name in SECRET_VARIABLES):
            raise Blocked("the command names a secret variable")
        if "/proc/" in token or "environ" in token:
            raise Blocked("reading process environments is blocked")
        if "cargo-lambda" in token:
            raise Blocked("cargo lambda (deploys) is blocked")
        value = token.split("=", 1)[-1] if token.startswith("-") else token
        if is_secret_file(value):
            raise Blocked("reading or writing .env, .pem or settings.local.json files is blocked")


def _is_punctuation(token: str) -> bool:
    return bool(token) and all(char in _PUNCTUATION for char in token)


def split_commands(tokens: list[str]) -> list[SimpleCommand]:
    """Simple commands, with their redirection targets pulled out."""
    commands = [SimpleCommand()]
    index = 0
    while index < len(tokens):
        token = tokens[index]
        if not _is_punctuation(token):
            commands[-1].words.append(token)
        elif "<" in token or ">" in token:
            index = _take_redirection(tokens, index, commands[-1])
        else:
            commands.append(SimpleCommand())
        index += 1
    return [command for command in commands if command.words or command.writes or command.reads]


def _take_redirection(tokens: list[str], index: int, command: SimpleCommand) -> int:
    operator = tokens[index]
    target = tokens[index + 1] if index + 1 < len(tokens) else ""
    if not target or _is_punctuation(target):
        raise Blocked("a redirection without a target cannot be checked")
    # A file descriptor number before the operator (`2>`) is not a word.
    if command.words and command.words[-1].isdigit():
        command.words.pop()
    if operator.endswith("&") and (target.isdigit() or target == "-"):
        return index + 1  # `2>&1`: duplicates a descriptor, no file
    if operator.startswith("<<"):
        return index + 1  # here-doc or here-string: no file
    if "<" in operator:
        command.reads.append(target)
    if ">" in operator:
        command.writes.append(target)
    return index + 1


def _check_simple(simple: SimpleCommand, cwd: str | None, workspace: str) -> str | None:
    words = _strip_prefixes(simple.words)
    for target in simple.writes:
        _check_write(target, cwd, workspace)
    for source in simple.reads:
        _check_inside(source, cwd, workspace, "reading a file outside the workspace is blocked")
    if not words:
        return cwd
    name = PurePosixPath(words[0]).name
    if words[0].startswith("$"):
        raise Blocked("a command name from a variable is blocked")
    if name in _BLOCKED_COMMANDS:
        raise Blocked(f"`{name}`: {_BLOCKED_COMMANDS[name]}")
    args = words[1:]
    _check_by_name(name, args, simple, cwd, workspace)
    if name == "cd":
        return _change_directory(args, cwd)
    return cwd


def _strip_prefixes(words: list[str]) -> list[str]:
    """Drops `NAME=value` assignments and transparent wrappers (`timeout 5`)."""
    rest = list(words)
    while rest and _ASSIGNMENT.match(rest[0]):
        rest.pop(0)
    while rest and PurePosixPath(rest[0]).name in _TRANSPARENT:
        rest.pop(0)
        while rest and (rest[0].startswith("-") or _DURATION.fullmatch(rest[0])):
            rest.pop(0)
    return rest


def _check_by_name(
    name: str, args: list[str], simple: SimpleCommand, cwd: str | None, workspace: str
) -> None:
    if name == "git":
        _check_git(args, cwd, workspace)
    elif name == "cargo":
        _check_cargo(args)
    elif name == "sqlx" and "migrate" in args:
        raise Blocked("`sqlx migrate` is blocked: migrations are applied by a person")
    elif name == "rm":
        _check_rm(args, cwd, workspace)
    elif name == "rg" and any(arg == "--pre" or arg.startswith("--pre=") for arg in args):
        raise Blocked("`rg --pre` runs commands and is blocked")
    elif name.startswith("mysql"):
        _check_mysql(args, simple)
    if name in _PACKAGE_RUNNERS:
        _check_package_runner(name, args, cwd, workspace)


def _check_git(args: list[str], cwd: str | None, workspace: str) -> None:
    index = 0
    while index < len(args) and args[index].startswith("-"):
        option = args[index]
        if option in ("-c", "--config-env") or option.startswith("--config-env="):
            raise Blocked("`git -c` is blocked")
        index += 2 if option == "-C" else 1
    subcommand = args[index] if index < len(args) else ""
    if subcommand in _GIT_BLOCKED:
        raise Blocked(f"`git {subcommand}` is blocked")
    for arg in args:
        if arg.startswith("--output"):
            raise Blocked("`git --output` is blocked: use the file tools to write files")


def _check_cargo(args: list[str]) -> None:
    for arg in args:
        if arg == "--config" or arg.startswith("--config="):
            raise Blocked("`cargo --config` is blocked")
    positional = [arg for arg in args if not arg.startswith("-")]
    subcommand = positional[0] if positional else ""
    if subcommand == "lambda":
        raise Blocked("`cargo lambda` (deploys) is blocked")
    if subcommand == "sqlx" and "migrate" in positional:
        raise Blocked("`sqlx migrate` is blocked: migrations are applied by a person")


def _check_package_runner(name: str, args: list[str], cwd: str | None, workspace: str) -> None:
    if any("deploy" in arg for arg in args):
        raise Blocked("deploy targets and scripts are blocked")
    if name == "bun" and args[:1] == ["test"]:
        raise Blocked("`bun test` is blocked: use `bunx vitest run <files>`")
    if (
        name == "bunx"
        and args[:1] == ["biome"]
        and any(arg in ("--write", "--fix", "--apply") for arg in args)
    ):
        for path in args[2:]:
            if not path.startswith("-"):
                _check_write(path, cwd, workspace)


def _check_rm(args: list[str], cwd: str | None, workspace: str) -> None:
    if not any(_is_recursive_flag(arg) for arg in args):
        return
    for path in args:
        if path.startswith("-"):
            continue
        if ".." in PurePosixPath(path).parts:
            raise Blocked("`rm -r` on a path with `..` is blocked")
        _check_inside(path, cwd, workspace, "`rm -r` outside the workspace is blocked")


def _is_recursive_flag(arg: str) -> bool:
    if arg == "--recursive":
        return True
    return re.fullmatch(r"-[A-Za-z]+", arg) is not None and ("r" in arg or "R" in arg)


def _check_mysql(args: list[str], simple: SimpleCommand) -> None:
    host, port = _mysql_target(args)
    if host != "127.0.0.1" or port != "3307":
        raise Blocked("mysql may only connect to -h 127.0.0.1 -P 3307")
    if simple.reads:
        raise Blocked("mysql may not read SQL from a file")
    for arg in args:
        if (
            "\\!" in arg
            or "\\." in arg
            or re.search(r"(^|;)\s*(system|source)\b", arg, re.IGNORECASE)
        ):
            raise Blocked("mysql `system` and `source` are blocked")


def _mysql_target(args: list[str]) -> tuple[str | None, str | None]:
    host: str | None = None
    port: str | None = None
    for index, arg in enumerate(args):
        following = args[index + 1] if index + 1 < len(args) else None
        if arg.startswith(("-S", "--socket", "--defaults", "--login-path", "--protocol")):
            raise Blocked("mysql sockets, option files and protocols are blocked")
        if arg in ("-h", "--host"):
            host = following
        elif arg.startswith("--host="):
            host = arg.split("=", 1)[1]
        elif arg.startswith("-h"):
            host = arg[2:]
        elif arg in ("-P", "--port"):
            port = following
        elif arg.startswith("--port="):
            port = arg.split("=", 1)[1]
        elif arg.startswith("-P"):
            port = arg[2:]
    return host, port


def _change_directory(args: list[str], cwd: str | None) -> str | None:
    """The directory after `cd`; None when it cannot be known (`cd -`, `cd ~`)."""
    target = args[0] if args else "~"
    if cwd is None or target.startswith("~") or target == "-":
        return None
    return os.path.normpath(os.path.join(cwd, target))


def _resolve(path: str, cwd: str | None) -> str | None:
    if path.startswith("~"):
        return None
    if not os.path.isabs(path):
        if cwd is None:
            return None
        path = os.path.join(cwd, path)
    return os.path.realpath(path)


def inside(path: str | None, workspace: str) -> bool:
    if path is None or not workspace:
        return False
    root = os.path.realpath(workspace)
    return path == root or path.startswith(root + os.sep)


def _check_inside(path: str, cwd: str | None, workspace: str, reason: str) -> str:
    resolved = _resolve(path, cwd)
    if resolved is None or not inside(resolved, workspace):
        raise Blocked(reason)
    return resolved


def _check_write(target: str, cwd: str | None, workspace: str) -> None:
    if target == "/dev/null":
        return
    resolved = _check_inside(target, cwd, workspace, "writing outside the workspace is blocked")
    relative = os.path.relpath(resolved, os.path.realpath(workspace))
    if is_write_protected(relative):
        raise Blocked(f"writing {relative} is blocked: it is a protected path")
