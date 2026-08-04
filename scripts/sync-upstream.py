from dataclasses import dataclass
from datetime import date
from urllib.parse import urlsplit


@dataclass(frozen=True)
class Commit:
    oid: str
    short_oid: str
    date: str
    author: str
    subject: str


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
