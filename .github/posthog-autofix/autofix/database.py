"""The throwaway MySQL both jobs build and test against.

`.cargo/config.toml` pins DATABASE_URL to 127.0.0.1:3307/granite_local:
sqlx's query! macros check queries against it at compile time, and the
api's tests create their databases on it. Runs as the runner user, before
the agent, on `main`'s migrations only.
"""

import subprocess
import time
from pathlib import Path

from . import proc

CONTAINER = "mysql"
CLIENT = ["mysql", "-h", "127.0.0.1", "-P", "3307", "-uroot", "-pgranite"]
# Durability traded for speed, as in docker-compose.local.yml.
SERVER_FLAGS = [
    "--log-bin-trust-function-creators=1",
    "--skip-log-bin",
    "--innodb-flush-log-at-trx-commit=0",
    "--sync-binlog=0",
    "--innodb-doublewrite=0",
    "--innodb-flush-method=nosync",
]
READY_SECONDS = 120


class DatabaseError(RuntimeError):
    pass


def start_argv() -> list[str]:
    return [
        "docker", "run", "-d", "--name", CONTAINER, "-p", "3307:3306",
        "-e", "MYSQL_ROOT_PASSWORD=granite", "-e", "MYSQL_DATABASE=granite_local",
        "mysql:8.0", *SERVER_FLAGS,
    ]  # fmt: skip


def start(runner: proc.Runner = proc.run) -> None:
    """Starts the container in the background, so pulling and initializing
    it overlaps with the setup steps that follow."""
    completed = runner(start_argv(), cwd=Path("/"), timeout=300)
    if not completed.ok:
        raise DatabaseError(f"docker run failed: {completed.output.strip()}")


def wait_until_ready(runner: proc.Runner = proc.run) -> None:
    ping = ["docker", "exec", CONTAINER, "mysqladmin", "ping", "-h", "127.0.0.1", "-pgranite"]
    for _ in range(READY_SECONDS):
        if runner(ping, cwd=Path("/"), timeout=10).ok:
            return
        time.sleep(1)
    raise DatabaseError(f"MySQL was not ready after {READY_SECONDS}s")


def migrate(repo: Path) -> None:
    """Applies every migration of `repo` in name order."""
    wait_until_ready()
    for migration in sorted((repo / "migrations").glob("*.sql")):
        with migration.open("rb") as sql:
            completed = subprocess.run(
                [*CLIENT, "granite_local"], stdin=sql, capture_output=True, check=False
            )
        if completed.returncode != 0:
            message = completed.stderr.decode(errors="replace").strip()
            raise DatabaseError(f"migration {migration.name} failed: {message}")
    print(f"Applied the migrations of {repo.name}.")
