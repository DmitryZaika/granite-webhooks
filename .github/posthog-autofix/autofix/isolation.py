"""The agent's sandbox in the fix job: its own Unix user, firewalled.

Everything the agent controls (its repos, the code its tests run, their
`.git`) is only ever executed or read by the `autofix` user, whose egress
reaches the Anthropic API and two pinned hosts and nothing else. The runner
user keeps its network for callbacks and the artifact upload.

Fixed layout, shared with the workflow (a test pins the two together):

    /srv/autofix/tools     these tools: the runner's, read-only to the agent
    /srv/autofix/work      granite-webhooks/ and general_datebase/
    /srv/autofix/cargo     CARGO_HOME, registry fetched before isolation
    /srv/autofix/rustup    RUSTUP_HOME, root-owned and read-only after isolation
    /srv/autofix/bin       bun and bunx (setup-bun installs into the runner's home)
    /srv/autofix/handoff   the agent user's outbox (the fix bundle)
    /srv/autofix/env.json  the agent's PATH, recorded before isolation
"""

import ipaddress
import json
import os
import shutil
import socket
import stat
from collections.abc import Callable, Sequence
from pathlib import Path

from . import proc
from .context import REPOS

USER = "autofix"
ROOT = Path("/srv/autofix")
TOOLS = ROOT / "tools"
WORK = ROOT / "work"
CARGO_HOME = ROOT / "cargo"
RUSTUP_HOME = ROOT / "rustup"
BIN = ROOT / "bin"
HANDOFF = ROOT / "handoff"
ENV_FILE = ROOT / "env.json"

# https://platform.claude.com/docs/en/api/ip-addresses
ANTHROPIC_API_V4 = ipaddress.ip_network("160.79.104.0/23")
# Resolved once at setup and pinned in /etc/hosts: the agent user has no DNS.
# platform.claude.com refreshes a claude.ai OAuth token.
PINNED_HOSTS = ("api.anthropic.com", "platform.claude.com", "mcp.posthog.com")
SYSTEM_PATH = ("/usr/local/bin", "/usr/bin", "/bin")
PROBE = Path(__file__).with_name("probe.py")
_HANDOFF_LIMIT = 512 * 1024 * 1024

Resolver = Callable[[str], list[str]]
Which = Callable[[str], str | None]


class IsolationError(RuntimeError):
    pass


# --- before isolation (runner user, trusted inputs only) ----------------------


def lay_out(workspace: Path, tools_checkout: Path, runner: proc.Runner = proc.run) -> None:
    """Moves the tools and the checkouts from the runner's workspace to ROOT.

    The same paths in the prepare and fix jobs keep the restored Cargo target
    dir valid (cargo fingerprints hold absolute paths).
    """
    uid, gid = str(os.getuid()), str(os.getgid())
    argv = ["sudo", "-n", "install", "-d", "-o", uid, "-g", gid, "-m", "755", str(ROOT)]
    created = runner(argv, cwd=Path("/"), timeout=60)
    if not created.ok:
        raise IsolationError(f"could not create {ROOT}: {created.output.strip()}")
    shutil.move(str(tools_checkout / ".github" / "posthog-autofix"), str(TOOLS))
    shutil.rmtree(tools_checkout)
    WORK.mkdir(exist_ok=True)
    for repo in REPOS:
        if (workspace / repo).exists():
            shutil.move(str(workspace / repo), str(WORK / repo))


def ipv4_addresses(host: str) -> list[str]:
    infos = socket.getaddrinfo(host, 443, socket.AF_INET, socket.SOCK_STREAM)
    return sorted({str(info[4][0]) for info in infos})


def resolve_pins(resolve: Resolver = ipv4_addresses) -> list[tuple[str, str]]:
    """(host, IPv4) pairs for /etc/hosts and the firewall.

    The API's addresses must sit in Anthropic's published range: anything
    else means DNS is not what we expect, and the run stops.
    """
    pins: list[tuple[str, str]] = []
    for host in PINNED_HOSTS:
        addresses = resolve(host)
        if not addresses:
            raise IsolationError(f"{host} did not resolve")
        for address in addresses:
            ip = ipaddress.ip_address(address)
            if ip.version != 4:
                raise IsolationError(f"{host}: {address} is not IPv4")
            if host == "api.anthropic.com" and ip not in ANTHROPIC_API_V4:
                raise IsolationError(f"{host} resolved outside {ANTHROPIC_API_V4}: {address}")
            pins.append((host, address))
    return pins


def agent_path(sysroot: Path, which: Which = shutil.which) -> str:
    """The agent user's PATH: its own bin, the Rust toolchain, node and the
    claude CLI. None may live in a home directory the user cannot read."""
    directories = [str(BIN), str(sysroot / "bin")]
    if not sysroot.is_relative_to(RUSTUP_HOME):
        raise IsolationError(f"the Rust toolchain {sysroot} is not under {RUSTUP_HOME}")
    for tool in ("node", "claude"):
        found = which(tool)
        if found is None:
            raise IsolationError(f"{tool} is not installed")
        if found.startswith("/home/"):
            raise IsolationError(f"{tool} is under a home directory: {found}")
        directories.append(os.path.dirname(found))
    directories += SYSTEM_PATH
    return ":".join(dict.fromkeys(directories))


def isolate_argv(bun: str, runner_home: str, pins: Sequence[tuple[str, str]]) -> list[str]:
    script = TOOLS / "isolate.sh"
    pinned = [f"{host}={address}" for host, address in pins]
    return ["sudo", "-n", "bash", str(script), str(ROOT), USER, bun, runner_home, *pinned]


def isolate(runner: proc.Runner = proc.run, which: Which = shutil.which) -> None:
    """Records the agent's PATH, then runs isolate.sh (user, files, firewall)."""
    pins = resolve_pins()
    sysroot = runner(["rustc", "--print", "sysroot"], cwd=Path("/"), timeout=60)
    if not sysroot.ok:
        raise IsolationError(f"rustc --print sysroot failed: {sysroot.output.strip()}")
    ENV_FILE.write_text(json.dumps({"PATH": agent_path(Path(sysroot.stdout.strip()), which)}))
    bun = which("bun")
    if bun is None:
        raise IsolationError("bun is not installed")
    argv = isolate_argv(os.path.realpath(bun), str(Path.home()), pins)
    completed = runner(argv, cwd=Path("/"), timeout=600)
    print(completed.output)
    if not completed.ok:
        raise IsolationError("isolate.sh failed")


# --- after isolation ----------------------------------------------------------


def child_env(path: str) -> dict[str, str]:
    """Variables every command of the agent user starts with."""
    return {
        "PATH": path,
        "CARGO_HOME": str(CARGO_HOME),
        "RUSTUP_HOME": str(RUSTUP_HOME),
        # Nothing may be fetched: the registry was filled before isolation.
        "CARGO_NET_OFFLINE": "true",
        "LANG": "C.UTF-8",
    }


def agent_user(secrets: dict[str, str] | None = None) -> proc.RunAs:
    recorded = json.loads(ENV_FILE.read_text())
    return proc.RunAs(USER, env=child_env(recorded["PATH"]), secrets=secrets or {})


def kill_leftovers(run_as: proc.RunAs, runner: proc.Runner = proc.run) -> None:
    """Stops whatever the agent user left running (a server a test started),
    so nothing of it outlives the step that ran it. Exit 1 means none."""
    runner(run_as.kill_argv(), cwd=Path("/"), timeout=60)


def copy_out(source: Path, target: Path) -> None:
    """Copies a file the agent user wrote, refusing a symlink or a special
    file (which could point at one of the runner's own files)."""
    descriptor = os.open(source, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor, "rb") as handle:
        info = os.fstat(handle.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_size > _HANDOFF_LIMIT:
            raise IsolationError(f"{source} is not a regular file of a sane size")
        target.write_bytes(handle.read())


# --- checking the sandbox -----------------------------------------------------


def network_probes(blocked_ip: str) -> list[tuple[bool, list[str]]]:
    """(must succeed, probe.py arguments). `blocked_ip` is an address the
    runner resolved, so a block is the firewall's doing and not only DNS's."""
    return [
        (False, ["tcp", "1.1.1.1", "443"]),
        (False, ["tcp", blocked_ip, "443"]),
        (False, ["tcp", "github.com", "443"]),
        (False, ["resolve", "example.com"]),
        (False, ["udp-dns", "8.8.8.8"]),
        (False, ["unix", "/var/run/docker.sock"]),
        (True, ["tcp", "127.0.0.1", "3307"]),
        (True, ["tcp", "api.anthropic.com", "443"]),
    ]


TOOL_PROBES: list[tuple[bool, list[str]]] = [
    (True, ["claude", "--version"]),
    (True, ["cargo", "--version"]),
    (True, ["cargo", "clippy", "--version"]),
    (True, ["bun", "--version"]),
    (True, ["bunx", "--version"]),
    (True, ["git", "--version"]),
    (False, ["sudo", "-n", "true"]),
]


def check(
    run_as: proc.RunAs,
    probes: Sequence[tuple[bool, list[str]]],
    runner: proc.Runner = proc.run,
) -> list[str]:
    """Runs each probe as the agent user; returns the ones that misbehaved."""
    failures = []
    for must_succeed, argv in probes:
        if argv[0] in ("tcp", "resolve", "udp-dns", "unix"):
            argv = ["python3", "-I", str(PROBE), *argv]
        completed = runner(argv, cwd=Path("/"), timeout=30, run_as=run_as)
        if completed.ok != must_succeed:
            expected = "succeed" if must_succeed else "fail"
            failures.append(f"`{' '.join(argv)}` should {expected} as {run_as.user}")
    return failures


def check_isolation(runner: proc.Runner = proc.run) -> list[str]:
    run_as = agent_user()
    probes = network_probes(ipv4_addresses("github.com")[0]) + TOOL_PROBES
    return check(run_as, probes, runner)
