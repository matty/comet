import argparse
from dataclasses import dataclass
from datetime import date
import json
from pathlib import Path
import re
import subprocess
import sys
from typing import Callable, Sequence, TextIO
from urllib.parse import urlsplit


@dataclass(frozen=True)
class Commit:
    oid: str
    short_oid: str
    date: str
    author: str
    subject: str


OUTCOMES = frozenset({"implemented", "not-applicable", "deferred"})
FULL_SHA = re.compile(r"^[0-9a-f]{40}$")


@dataclass(frozen=True)
class LedgerEntry:
    upstream_sha: str
    subject: str
    outcome: str
    decision_date: str
    note: str
    local_commit: str | None = None


@dataclass(frozen=True)
class RunDecision:
    upstream_sha: str
    subject: str
    outcome: str
    note: str
    local_commit: str | None = None


@dataclass(frozen=True)
class SyncRun:
    run_id: str
    kind: str
    date: str
    target_branch: str
    sync_branch: str | None
    decisions: tuple[RunDecision, ...]


@dataclass(frozen=True)
class Ledger:
    schema_version: int
    commits: dict[str, LedgerEntry]
    runs: tuple[SyncRun, ...]


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
                encoding="utf-8",
                errors="replace",
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
        except FileNotFoundError as error:
            raise SyncError("Git is not installed or is not on PATH.") from error
        if check and result.returncode != 0:
            raise GitError(list(args), result.returncode, result.stdout, result.stderr)
        return result


def _require_object(value: object, context: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise SyncError(f"{context} must be a JSON object.")
    return value


def _require_keys(
    value: dict[str, object],
    required: set[str],
    context: str,
    optional: set[str] | None = None,
) -> None:
    optional = optional or set()
    missing = required - value.keys()
    extra = value.keys() - required - optional
    if missing:
        raise SyncError(
            f"{context} is missing fields: {', '.join(sorted(missing))}."
        )
    if extra:
        raise SyncError(
            f"{context} has unknown fields: {', '.join(sorted(extra))}."
        )


def _require_string(value: object, field: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise SyncError(f"{field} must be a non-empty string.")
    return value


def _require_sha(value: object, field: str) -> str:
    sha = _require_string(value, field)
    if FULL_SHA.fullmatch(sha) is None:
        raise SyncError(f"{field} must be a full lowercase SHA.")
    return sha


def _require_date(value: object, field: str) -> str:
    text = _require_string(value, field)
    try:
        date.fromisoformat(text)
    except ValueError as error:
        raise SyncError(f"{field} must use YYYY-MM-DD format.") from error
    return text


def _parse_decision(value: object, context: str) -> RunDecision:
    data = _require_object(value, context)
    _require_keys(
        data,
        {"upstream_sha", "subject", "outcome", "note"},
        context,
        {"local_commit"},
    )
    upstream_sha = _require_sha(data["upstream_sha"], f"{context}.upstream_sha")
    subject = _require_string(data["subject"], f"{context}.subject")
    outcome = _require_string(data["outcome"], f"{context}.outcome")
    if outcome not in OUTCOMES:
        raise SyncError(f"{context}.outcome has an unsupported outcome: {outcome}.")
    note = _require_string(data["note"], f"{context}.note")
    local_value = data.get("local_commit")
    local_commit = None
    if local_value is not None:
        local_commit = _require_sha(local_value, f"{context}.local_commit")
    if outcome != "implemented" and local_commit is not None:
        raise SyncError(
            f"{context}.local_commit is only valid for implemented commits."
        )
    return RunDecision(upstream_sha, subject, outcome, note, local_commit)


def _parse_entry(key: str, value: object) -> LedgerEntry:
    context = f"commits[{key}]"
    data = _require_object(value, context)
    _require_keys(
        data,
        {
            "upstream_sha",
            "subject",
            "outcome",
            "decision_date",
            "note",
        },
        context,
        {"local_commit"},
    )
    decision = _parse_decision(
        {
            name: data[name]
            for name in ("upstream_sha", "subject", "outcome", "note")
        }
        | ({"local_commit": data["local_commit"]} if "local_commit" in data else {}),
        context,
    )
    if key != decision.upstream_sha:
        raise SyncError(f"{context}.upstream_sha does not match its object key.")
    decision_date = _require_date(data["decision_date"], f"{context}.decision_date")
    return LedgerEntry(
        decision.upstream_sha,
        decision.subject,
        decision.outcome,
        decision_date,
        decision.note,
        decision.local_commit,
    )


def _parse_run(value: object, index: int) -> SyncRun:
    context = f"runs[{index}]"
    data = _require_object(value, context)
    _require_keys(
        data,
        {
            "run_id",
            "kind",
            "date",
            "target_branch",
            "sync_branch",
            "decisions",
        },
        context,
    )
    run_id = _require_string(data["run_id"], f"{context}.run_id")
    kind = _require_string(data["kind"], f"{context}.kind")
    if kind not in {"bootstrap", "sync"}:
        raise SyncError(f"{context}.kind must be bootstrap or sync.")
    run_date = _require_date(data["date"], f"{context}.date")
    target = _require_string(data["target_branch"], f"{context}.target_branch")
    sync_value = data["sync_branch"]
    if kind == "bootstrap":
        if sync_value is not None:
            raise SyncError(f"{context} bootstrap run must not have a sync branch.")
        sync_branch = None
    else:
        sync_branch = _require_string(sync_value, f"{context}.sync_branch")
    decisions_value = data["decisions"]
    if not isinstance(decisions_value, list) or not decisions_value:
        raise SyncError(f"{context}.decisions must be a non-empty list.")
    decisions = tuple(
        _parse_decision(item, f"{context}.decisions[{decision_index}]")
        for decision_index, item in enumerate(decisions_value)
    )
    return SyncRun(run_id, kind, run_date, target, sync_branch, decisions)


def load_ledger(path: Path) -> Ledger:
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except OSError as error:
        raise SyncError(f"Unable to read upstream sync ledger: {error}.") from error
    except json.JSONDecodeError as error:
        raise SyncError(f"Upstream sync ledger is invalid JSON: {error.msg}.") from error
    data = _require_object(document, "ledger")
    _require_keys(data, {"schema_version", "commits", "runs"}, "ledger")
    if data["schema_version"] != 1:
        raise SyncError(
            f"Unsupported upstream sync ledger schema version: "
            f"{data['schema_version']}."
        )
    commits_value = _require_object(data["commits"], "ledger.commits")
    commits = {key: _parse_entry(key, value) for key, value in commits_value.items()}
    runs_value = data["runs"]
    if not isinstance(runs_value, list):
        raise SyncError("ledger.runs must be a JSON list.")
    runs = tuple(_parse_run(value, index) for index, value in enumerate(runs_value))
    run_ids = [run.run_id for run in runs]
    if len(run_ids) != len(set(run_ids)):
        raise SyncError("Duplicate run ID in upstream sync ledger.")
    return Ledger(1, commits, runs)


def _decision_document(decision: RunDecision) -> dict[str, object]:
    return {
        "upstream_sha": decision.upstream_sha,
        "subject": decision.subject,
        "outcome": decision.outcome,
        "note": decision.note,
        "local_commit": decision.local_commit,
    }


def serialize_ledger(ledger: Ledger) -> str:
    document = {
        "schema_version": ledger.schema_version,
        "commits": {
            sha: {
                "upstream_sha": entry.upstream_sha,
                "subject": entry.subject,
                "outcome": entry.outcome,
                "decision_date": entry.decision_date,
                "note": entry.note,
                "local_commit": entry.local_commit,
            }
            for sha, entry in ledger.commits.items()
        },
        "runs": [
            {
                "run_id": run.run_id,
                "kind": run.kind,
                "date": run.date,
                "target_branch": run.target_branch,
                "sync_branch": run.sync_branch,
                "decisions": [
                    _decision_document(decision) for decision in run.decisions
                ],
            }
            for run in ledger.runs
        ],
    }
    return json.dumps(document, indent=2, sort_keys=True) + "\n"


def write_ledger(path: Path, ledger: Ledger) -> None:
    with path.open("w", encoding="utf-8", newline="\n") as output:
        output.write(serialize_ledger(ledger))


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
            f"Remote 'upstream' has an unexpected URL: {found_url}. "
            "Rename it first with git remote rename upstream <new-name>."
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


def filter_resolved(commits: list[Commit], ledger: Ledger) -> list[Commit]:
    return [
        commit
        for commit in commits
        if ledger.commits.get(commit.oid) is None
        or ledger.commits[commit.oid].outcome == "deferred"
    ]


def existing_branches(git: Git) -> set[str]:
    result = git.run(
        "for-each-ref",
        "--format=%(refname:short)",
        "refs/heads/",
    )
    return {line for line in result.stdout.splitlines() if line}


def parse_selection(
    text: str,
    commits: list[Commit],
    allow_empty: bool = False,
) -> list[Commit]:
    if not text.strip():
        if allow_empty:
            return []
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


def classify_unselected(
    commits: list[Commit],
    selected_oids: set[str],
    input_fn: Callable[[str], str],
    output: TextIO,
) -> list[RunDecision]:
    decisions = []
    for commit in commits:
        if commit.oid in selected_oids:
            continue
        while True:
            answer = input_fn(
                f"Commit {commit.short_oid} was not selected: "
                "[d]efer/[n]ot-applicable [d] "
            ).strip().lower()
            if answer in {"", "d", "defer", "deferred"}:
                decisions.append(
                    RunDecision(
                        commit.oid,
                        commit.subject,
                        "deferred",
                        "Deferred during interactive review.",
                    )
                )
                break
            if answer in {"n", "not-applicable"}:
                while True:
                    reason = input_fn(
                        "Reason this commit is not applicable: "
                    ).strip()
                    if reason:
                        decisions.append(
                            RunDecision(
                                commit.oid,
                                commit.subject,
                                "not-applicable",
                                reason,
                            )
                        )
                        break
                    print("Reason is required.", file=output)
                break
            print("Choose d or n.", file=output)
    return decisions


def build_sync_run(
    day: date,
    target: str,
    sync_branch: str,
    commits: list[Commit],
    local_commits: dict[str, str],
    classifications: list[RunDecision],
) -> SyncRun:
    classifications_by_oid = {
        decision.upstream_sha: decision for decision in classifications
    }
    known_oids = {commit.oid for commit in commits}
    supplied_oids = set(local_commits) | set(classifications_by_oid)
    if supplied_oids != known_oids:
        raise SyncError("Run decisions do not match the discovered commits.")
    decisions = []
    for commit in reversed(commits):
        local_commit = local_commits.get(commit.oid)
        if local_commit is not None:
            decisions.append(
                RunDecision(
                    commit.oid,
                    commit.subject,
                    "implemented",
                    "Cherry-picked by upstream sync helper.",
                    local_commit,
                )
            )
        else:
            decisions.append(classifications_by_oid[commit.oid])
    return SyncRun(
        sync_branch,
        "sync",
        day.isoformat(),
        target,
        sync_branch,
        tuple(decisions),
    )


def apply_run(ledger: Ledger, run: SyncRun) -> Ledger:
    if any(existing.run_id == run.run_id for existing in ledger.runs):
        raise SyncError(f"Duplicate run ID in upstream sync ledger: {run.run_id}.")
    commits = dict(ledger.commits)
    for decision in run.decisions:
        commits[decision.upstream_sha] = LedgerEntry(
            decision.upstream_sha,
            decision.subject,
            decision.outcome,
            run.date,
            decision.note,
            decision.local_commit,
        )
    return Ledger(ledger.schema_version, commits, ledger.runs + (run,))


def next_branch_name(existing: set[str], day: date) -> str:
    base = f"sync/upstream-{day.isoformat()}"
    if base not in existing:
        return base
    suffix = 2
    while f"{base}-{suffix}" in existing:
        suffix += 1
    return f"{base}-{suffix}"


def format_commit(index: int | None, commit: Commit) -> str:
    prefix = f"{index}. " if index is not None else ""
    return (
        f"{prefix}{commit.short_oid}  {commit.date}  "
        f"{commit.author}  {commit.subject}"
    )


def format_failure_detail(stdout: str, stderr: str, returncode: int) -> str:
    for captured in (stderr, stdout):
        detail = " ".join(captured.split())
        if detail:
            return detail
    return f"exit status {returncode}"


def run_workflow(
    git: Git,
    input_fn: Callable[[str], str],
    output: TextIO,
    day: date,
) -> int:
    target = validate_repository(git)
    ensure_upstream(git)
    git.run("fetch", "--prune", "upstream", "main")
    commits = discover_commits(git, target)

    if not commits:
        print(f"{target} is already aligned with upstream/main.", file=output)
        return 0

    print("Eligible upstream commits (newest first):", file=output)
    for index, commit in enumerate(commits, start=1):
        print(format_commit(index, commit), file=output)

    while True:
        try:
            selected = parse_selection(
                input_fn("Select commits (for example 1,3-5): "), commits
            )
            break
        except ValueError as error:
            print(error, file=output)

    print("Selected commits (oldest first):", file=output)
    for commit in selected:
        print(format_commit(None, commit), file=output)

    confirmation = input_fn(
        "Create a sync branch and cherry-pick these commits? [y/N] "
    )
    if confirmation.strip().lower() not in {"y", "yes"}:
        return 0

    branch = next_branch_name(existing_branches(git), day)
    git.run("switch", "-c", branch)
    for commit in selected:
        result = git.run("cherry-pick", commit.oid, check=False)
        if result.returncode != 0:
            detail = format_failure_detail(
                result.stdout, result.stderr, result.returncode
            )
            print(
                f"Cherry-pick stopped at {commit.short_oid} "
                f"({commit.subject}): {detail}",
                file=output,
            )
            print("Inspect the repository with git status.", file=output)
            print(
                "If conflicts or cherry-pick state are present, resolve them "
                "and choose:",
                file=output,
            )
            print("git add <resolved-files>", file=output)
            print("git cherry-pick --continue", file=output)
            print("git cherry-pick --abort", file=output)
            return 1

    print("Review and integrate the sync branch with:", file=output)
    print(f"git diff {target}...HEAD", file=output)
    print(f"git log --oneline {target}..HEAD", file=output)
    print(f"git switch {target}", file=output)
    print(f"git merge --ff-only {branch}", file=output)
    return 0


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


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Select and cherry-pick commits from zeronsh/comet",
        epilog=(
            "Run from a clean worktree with an attached branch. The helper "
            "configures the fixed upstream remote when missing and refuses a "
            "collision when that name points elsewhere.\n"
            "Choose commits with 2, 1,4, or 2-5, then review the oldest-first "
            "order and give confirmation before any branch is created.\n"
            "The helper creates a sync/upstream-YYYY-MM-DD safety branch. "
            "If a cherry-pick stops, resolve conflicts manually and continue "
            "or abort it yourself.\n"
            "On success, it prints integration commands for review and a "
            "fast-forward merge; it never merges or pushes."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.parse_args(argv)

    try:
        git = Git()
        git.cwd = repository_root(git)
        return run_workflow(git, input, sys.stdout, date.today())
    except GitError as error:
        command = "git " + " ".join(error.args_list)
        detail = format_failure_detail(
            error.stdout, error.stderr, error.returncode
        )
        print(f"error: {command} failed: {detail}", file=sys.stderr)
        return 1
    except SyncError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
