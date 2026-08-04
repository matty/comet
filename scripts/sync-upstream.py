from dataclasses import dataclass
from datetime import date
from pathlib import Path
import subprocess
from urllib.parse import urlsplit


@dataclass(frozen=True)
class Commit:
    oid: str
    short_oid: str
    date: str
    author: str
    subject: str


class SyncError(RuntimeError):
    pass


class GitError(RuntimeError):
    def __init__(
        self,
        args_list: list[str],
        returncode: int,
        stdout: str,
        stderr: str,
    ) -> None:
        super().__init__(stderr.strip() or f"Git exited with status {returncode}.")
        self.args_list = args_list
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


class Git:
    def __init__(self, cwd: Path | None = None) -> None:
        self.cwd = cwd

    def run(
        self, *args: str, check: bool = True
    ) -> subprocess.CompletedProcess[str]:
        try:
            result = subprocess.run(
                ["git", *args],
                cwd=self.cwd,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
        except FileNotFoundError as error:
            raise SyncError("Git is not installed or is not on PATH.") from error
        if check and result.returncode != 0:
            raise GitError(list(args), result.returncode, result.stdout, result.stderr)
        return result


def repository_root(git: Git) -> Path:
    return Path(git.run("rev-parse", "--show-toplevel").stdout.strip())


def validate_repository(git: Git) -> str:
    if git.run("status", "--porcelain").stdout:
        raise SyncError("The repository must have a clean worktree.")
    try:
        branch = git.run("symbolic-ref", "--quiet", "--short", "HEAD")
    except GitError as error:
        raise SyncError("The repository is in detached HEAD state.") from error
    return branch.stdout.strip()


def ensure_upstream(git: Git) -> None:
    result = git.run("remote", "get-url", "upstream", check=False)
    if result.returncode != 0:
        git.run(
            "remote",
            "add",
            "upstream",
            "https://github.com/zeronsh/comet.git",
        )
        return
    found_url = result.stdout.strip()
    if not is_expected_upstream(found_url):
        raise SyncError(
            f"Remote 'upstream' has an unexpected URL: {found_url}"
        )


def discover_commits(git: Git, target: str) -> list[Commit]:
    result = git.run(
        "log",
        "--right-only",
        "--cherry-pick",
        "--topo-order",
        "--format=%H%x00%h%x00%cs%x00%an%x00%s",
        f"{target}...upstream/main",
    )
    commits = []
    for line in result.stdout.splitlines():
        fields = line.split("\x00")
        if len(fields) != 5:
            raise SyncError("Git returned malformed commit data.")
        commits.append(Commit(*fields))
    return commits


def existing_branches(git: Git) -> set[str]:
    result = git.run(
        "for-each-ref",
        "--format=%(refname:short)",
        "refs/heads/",
    )
    return {line for line in result.stdout.splitlines() if line}


def parse_selection(text: str, commits: list[Commit]) -> list[Commit]:
    if not text.strip():
        raise ValueError("Select at least one commit.")
    indexes: set[int] = set()
    for token in (part.strip() for part in text.split(",")):
        if not token:
            raise ValueError("Selection contains an empty item.")
        if "-" in token:
            bounds = token.split("-")
            if len(bounds) != 2 or not all(part.isdigit() for part in bounds):
                raise ValueError(f"Invalid range: {token}")
            start, end = map(int, bounds)
            if start > end:
                raise ValueError(f"Range must be ascending: {token}")
            indexes.update(range(start, end + 1))
        elif token.isdigit():
            indexes.add(int(token))
        else:
            raise ValueError(f"Invalid selection: {token}")
    if min(indexes) < 1 or max(indexes) > len(commits):
        raise ValueError(f"Choose numbers from 1 to {len(commits)}.")
    chosen = [commits[index - 1] for index in indexes]
    position = {commit.oid: index for index, commit in enumerate(commits)}
    return sorted(chosen, key=lambda commit: position[commit.oid], reverse=True)


def next_branch_name(existing: set[str], day: date) -> str:
    base = f"sync/upstream-{day.isoformat()}"
    if base not in existing:
        return base
    suffix = 2
    while f"{base}-{suffix}" in existing:
        suffix += 1
    return f"{base}-{suffix}"


def normalize_github_url(url: str) -> str:
    normalized = url.strip().lower().rstrip("/")
    if "://" in normalized:
        parsed = urlsplit(normalized)
        host = parsed.hostname or ""
        path = parsed.path
    elif normalized.startswith("git@") and ":" in normalized:
        authority, path = normalized.split(":", 1)
        host = authority.rsplit("@", 1)[-1]
    else:
        host, _, path = normalized.partition("/")
    path = path.strip("/")
    if path.endswith(".git"):
        path = path[:-4]
    return f"{host}/{path}" if path else host


def is_expected_upstream(url: str) -> bool:
    return normalize_github_url(url) == "github.com/zeronsh/comet"
