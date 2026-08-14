import argparse
from dataclasses import dataclass, replace
from datetime import date
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
from typing import Callable, Sequence, TextIO
from urllib.parse import urlsplit


@dataclass(frozen=True)
class Commit:
    oid: str
    short_oid: str
    date: str
    author: str
    subject: str


@dataclass(frozen=True)
class HeadState:
    oid: str
    parents: tuple[str, ...]
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


@dataclass(frozen=True)
class PendingRun:
    schema_version: int
    phase: str
    target_branch: str
    sync_branch: str
    run_id: str
    run_date: str
    selected: tuple[Commit, ...]
    classifications: tuple[RunDecision, ...]
    local_commits: dict[str, str]
    active_upstream_sha: str | None = None
    pre_pick_head: str | None = None
    considered: tuple[Commit, ...] = ()


class SyncError(RuntimeError):
    pass


class SyncCancelled(SyncError):
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


class GhError(RuntimeError):
    def __init__(
        self,
        args_list: list[str],
        returncode: int,
        stdout: str,
        stderr: str,
    ) -> None:
        super().__init__(stderr.strip() or f"GitHub CLI exited with status {returncode}.")
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


class Gh:
    def __init__(self, cwd: Path | None = None) -> None:
        self.cwd = cwd

    def run(
        self, *args: str, check: bool = True
    ) -> subprocess.CompletedProcess[str]:
        try:
            result = subprocess.run(
                ["gh", *args],
                cwd=self.cwd,
                text=True,
                encoding="utf-8",
                errors="replace",
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
        except FileNotFoundError as error:
            raise SyncError(
                "GitHub CLI is not installed or is not on PATH."
            ) from error
        if check and result.returncode != 0:
            raise GhError(list(args), result.returncode, result.stdout, result.stderr)
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
    serialized = serialize_ledger(ledger)
    temporary = path.with_name(path.name + ".tmp")
    with temporary.open("w", encoding="utf-8", newline="\n") as output:
        output.write(serialized)
    temporary.replace(path)


def _commit_document(commit: Commit) -> dict[str, str]:
    return {
        "oid": commit.oid,
        "short_oid": commit.short_oid,
        "date": commit.date,
        "author": commit.author,
        "subject": commit.subject,
    }


def _parse_commit(value: object, context: str) -> Commit:
    data = _require_object(value, context)
    _require_keys(
        data,
        {"oid", "short_oid", "date", "author", "subject"},
        context,
    )
    oid = _require_sha(data["oid"], f"{context}.oid")
    short_oid = _require_string(data["short_oid"], f"{context}.short_oid")
    if short_oid != oid[: len(short_oid)] or len(short_oid) < 7:
        raise SyncError(f"{context}.short_oid must abbreviate its full SHA.")
    commit_date = _require_date(data["date"], f"{context}.date")
    author = _require_string(data["author"], f"{context}.author")
    subject = _require_string(data["subject"], f"{context}.subject")
    return Commit(oid, short_oid, commit_date, author, subject)


def _validate_pending(pending: PendingRun) -> None:
    if pending.schema_version != 1:
        raise SyncError(
            f"Unsupported pending sync schema version: {pending.schema_version}."
        )
    if pending.phase not in {
        "prepared",
        "cherry-picking",
        "ledger-committed",
        "pushed",
    }:
        raise SyncError(f"Unsupported pending sync phase: {pending.phase}.")
    _require_string(pending.target_branch, "pending.target_branch")
    _require_string(pending.sync_branch, "pending.sync_branch")
    _require_string(pending.run_id, "pending.run_id")
    _require_date(pending.run_date, "pending.run_date")
    if pending.run_id != pending.sync_branch:
        raise SyncError("Pending run ID must match its sync branch.")
    selected_oids = [commit.oid for commit in pending.selected]
    if len(selected_oids) != len(set(selected_oids)):
        raise SyncError("Pending selected commits contain duplicates.")
    classification_oids = [
        decision.upstream_sha for decision in pending.classifications
    ]
    if len(classification_oids) != len(set(classification_oids)):
        raise SyncError("Pending classifications contain duplicates.")
    if set(selected_oids) & set(classification_oids):
        raise SyncError("A pending commit cannot be selected and classified.")
    considered_oids = [commit.oid for commit in pending.considered]
    if len(considered_oids) != len(set(considered_oids)):
        raise SyncError("Pending considered commits contain duplicates.")
    if set(considered_oids) != set(selected_oids) | set(classification_oids):
        raise SyncError(
            "Pending considered commits must match selections and classifications."
        )
    if any(
        decision.outcome not in {"deferred", "not-applicable"}
        or decision.local_commit is not None
        for decision in pending.classifications
    ):
        raise SyncError("Pending classifications must be deferred or not-applicable.")
    for upstream_sha, local_commit in pending.local_commits.items():
        _require_sha(upstream_sha, "pending.local_commits upstream SHA")
        _require_sha(local_commit, "pending.local_commits local SHA")
    completed_count = len(pending.local_commits)
    expected_completed = set(selected_oids[:completed_count])
    if set(pending.local_commits) != expected_completed:
        raise SyncError(
            "Pending local commits must match a chronological prefix of selections."
        )
    active_pair = (
        pending.active_upstream_sha is not None,
        pending.pre_pick_head is not None,
    )
    if active_pair[0] != active_pair[1]:
        raise SyncError("Pending active commit and pre-pick HEAD must be paired.")
    if pending.active_upstream_sha is not None:
        _require_sha(pending.active_upstream_sha, "pending.active_upstream_sha")
        _require_sha(pending.pre_pick_head, "pending.pre_pick_head")
        if pending.phase != "cherry-picking":
            raise SyncError("Only cherry-picking may have an active commit.")
        if completed_count >= len(selected_oids):
            raise SyncError("Pending active commit has no remaining selection.")
        if pending.active_upstream_sha != selected_oids[completed_count]:
            raise SyncError("Pending active commit is not the next selection.")
    if pending.phase == "prepared":
        if pending.local_commits or pending.active_upstream_sha is not None:
            raise SyncError("A prepared run cannot contain completed picks.")
    if pending.phase in {"ledger-committed", "pushed"}:
        if completed_count != len(selected_oids):
            raise SyncError(
                f"A {pending.phase} run must contain every local commit mapping."
            )
        if pending.active_upstream_sha is not None:
            raise SyncError(f"A {pending.phase} run cannot have an active pick.")


def _pending_document(pending: PendingRun) -> dict[str, object]:
    return {
        "schema_version": pending.schema_version,
        "phase": pending.phase,
        "target_branch": pending.target_branch,
        "sync_branch": pending.sync_branch,
        "run_id": pending.run_id,
        "run_date": pending.run_date,
        "selected": [_commit_document(commit) for commit in pending.selected],
        "classifications": [
            _decision_document(decision) for decision in pending.classifications
        ],
        "local_commits": pending.local_commits,
        "active_upstream_sha": pending.active_upstream_sha,
        "pre_pick_head": pending.pre_pick_head,
        "considered": [
            _commit_document(commit) for commit in pending.considered
        ],
    }


def _parse_pending(value: object) -> PendingRun:
    data = _require_object(value, "pending")
    _require_keys(
        data,
        {
            "schema_version",
            "phase",
            "target_branch",
            "sync_branch",
            "run_id",
            "run_date",
            "selected",
            "classifications",
            "local_commits",
            "active_upstream_sha",
            "pre_pick_head",
            "considered",
        },
        "pending",
    )
    if not isinstance(data["selected"], list):
        raise SyncError("pending.selected must be a JSON list.")
    if not isinstance(data["classifications"], list):
        raise SyncError("pending.classifications must be a JSON list.")
    if not isinstance(data["considered"], list):
        raise SyncError("pending.considered must be a JSON list.")
    local_value = _require_object(data["local_commits"], "pending.local_commits")
    active = data["active_upstream_sha"]
    pre_pick = data["pre_pick_head"]
    if active is not None and not isinstance(active, str):
        raise SyncError("pending.active_upstream_sha must be a string or null.")
    if pre_pick is not None and not isinstance(pre_pick, str):
        raise SyncError("pending.pre_pick_head must be a string or null.")
    pending = PendingRun(
        data["schema_version"],
        _require_string(data["phase"], "pending.phase"),
        _require_string(data["target_branch"], "pending.target_branch"),
        _require_string(data["sync_branch"], "pending.sync_branch"),
        _require_string(data["run_id"], "pending.run_id"),
        _require_date(data["run_date"], "pending.run_date"),
        tuple(
            _parse_commit(item, f"pending.selected[{index}]")
            for index, item in enumerate(data["selected"])
        ),
        tuple(
            _parse_decision(item, f"pending.classifications[{index}]")
            for index, item in enumerate(data["classifications"])
        ),
        dict(local_value),
        active,
        pre_pick,
        tuple(
            _parse_commit(item, f"pending.considered[{index}]")
            for index, item in enumerate(data["considered"])
        ),
    )
    _validate_pending(pending)
    return pending


def load_pending(path: Path) -> PendingRun:
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except OSError as error:
        raise SyncError(f"Unable to read pending upstream sync state: {error}.") from error
    except json.JSONDecodeError as error:
        raise SyncError(f"Pending upstream sync state is invalid JSON: {error.msg}.") from error
    return _parse_pending(document)


def write_pending(path: Path, pending: PendingRun) -> None:
    _validate_pending(pending)
    serialized = json.dumps(
        _pending_document(pending), indent=2, sort_keys=True
    ) + "\n"
    temporary = path.with_name(path.name + ".tmp")
    with temporary.open("w", encoding="utf-8", newline="\n") as output:
        output.write(serialized)
    temporary.replace(path)


def clear_pending(path: Path) -> None:
    path.unlink(missing_ok=True)


def pending_state_path(git: Git) -> Path:
    value = git.run(
        "rev-parse", "--git-path", "upstream-sync-state.json"
    ).stdout.strip()
    if not value:
        raise SyncError("Git returned an empty pending-state path.")
    path = Path(value)
    if not path.is_absolute():
        path = (git.cwd or Path.cwd()) / path
    return path


def record_pick_start(
    pending: PendingRun,
    commit: Commit,
    pre_pick_head: str,
) -> PendingRun:
    if pending.phase != "cherry-picking" or pending.active_upstream_sha is not None:
        raise SyncError("Pending state is not ready to start a cherry-pick.")
    completed_count = len(pending.local_commits)
    if (
        completed_count >= len(pending.selected)
        or pending.selected[completed_count].oid != commit.oid
    ):
        raise SyncError("Cherry-pick is not the next pending selection.")
    _require_sha(pre_pick_head, "pre-pick HEAD")
    updated = replace(
        pending,
        active_upstream_sha=commit.oid,
        pre_pick_head=pre_pick_head,
    )
    _validate_pending(updated)
    return updated


def record_pick_success(pending: PendingRun, local_commit: str) -> PendingRun:
    if pending.active_upstream_sha is None:
        raise SyncError("Pending state has no active cherry-pick.")
    _require_sha(local_commit, "local cherry-pick SHA")
    mappings = dict(pending.local_commits)
    mappings[pending.active_upstream_sha] = local_commit
    updated = replace(
        pending,
        local_commits=mappings,
        active_upstream_sha=None,
        pre_pick_head=None,
    )
    _validate_pending(updated)
    return updated


def verify_resumed_pick(pending: PendingRun, head: HeadState) -> PendingRun:
    if pending.active_upstream_sha is None or pending.pre_pick_head is None:
        return pending
    _require_sha(head.oid, "current HEAD")
    if head.oid == pending.pre_pick_head:
        raise SyncCancelled("The active cherry-pick was aborted.")
    # A HEAD that merely moved is not proof the pick landed: the branch may
    # carry an unrelated commit made after an abort. Recording that commit
    # would mark the upstream change implemented and hide it forever.
    commit = pending.selected[len(pending.local_commits)]
    if head.parents != (pending.pre_pick_head,) or head.subject != commit.subject:
        raise SyncError(
            f"HEAD {head.oid} ({head.subject}) is not the cherry-pick of "
            f"{commit.short_oid} ({commit.subject}). Restore the original "
            "subject with git commit --amend if the pick did land, or reset "
            f"the branch to {pending.pre_pick_head} to redo it, then resume."
        )
    return record_pick_success(pending, head.oid)


def ensure_cherry_pick_resolved(git: Git) -> None:
    result = git.run(
        "rev-parse", "-q", "--verify", "CHERRY_PICK_HEAD", check=False
    )
    if result.returncode == 0:
        raise SyncError(
            "A cherry-pick is still in progress. Resolve it and run "
            "git cherry-pick --continue before resuming."
        )


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


def validate_origin(git: Git) -> None:
    result = git.run("remote", "get-url", "origin", check=False)
    if result.returncode != 0 or not result.stdout.strip():
        raise SyncError(
            "Remote 'origin' is required to push the sync branch and open a PR."
        )


def validate_github(gh: Gh) -> None:
    try:
        gh.run("auth", "status")
    except GhError as error:
        detail = format_failure_detail(
            error.stdout, error.stderr, error.returncode
        )
        raise SyncError(
            f"GitHub CLI authentication failed: {detail}. Run gh auth login."
        ) from error


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


def format_pr(run: SyncRun) -> tuple[str, str]:
    title = f"Sync upstream commits ({run.date})"
    headings = {
        "implemented": "Implemented",
        "not-applicable": "Not applicable",
        "deferred": "Deferred",
    }
    sections = []
    for outcome in ("implemented", "not-applicable", "deferred"):
        decisions = [
            decision for decision in run.decisions if decision.outcome == outcome
        ]
        if not decisions:
            continue
        lines = [f"## {headings[outcome]}", ""]
        for decision in decisions:
            upstream = decision.upstream_sha[:7]
            if decision.local_commit is not None:
                prefix = f"`{upstream}` → `{decision.local_commit[:7]}`"
            else:
                prefix = f"`{upstream}`"
            lines.append(f"- {prefix} — {decision.subject}")
            if outcome != "implemented":
                lines.append(f"  - {decision.note}")
        sections.append("\n".join(lines))
    return title, "\n\n".join(sections) + "\n"


def find_existing_pr(gh: Gh, head: str, base: str) -> str | None:
    result = gh.run(
        "pr",
        "list",
        "--state",
        "open",
        "--base",
        base,
        "--head",
        head,
        "--json",
        "url",
    )
    try:
        matches = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise SyncError("GitHub CLI returned malformed PR data.") from error
    if not isinstance(matches, list):
        raise SyncError("GitHub CLI returned malformed PR data.")
    if not matches:
        return None
    if len(matches) != 1:
        raise SyncError("Multiple open PRs match the sync branch and target.")
    match = matches[0]
    if (
        not isinstance(match, dict)
        or not isinstance(match.get("url"), str)
        or not match["url"].strip()
    ):
        raise SyncError("GitHub CLI returned malformed PR data.")
    return match["url"]


def create_draft_pr(
    gh: Gh,
    run: SyncRun,
    title: str,
    body: str,
) -> str:
    if run.sync_branch is None:
        raise SyncError("A bootstrap run cannot create a pull request.")
    with tempfile.TemporaryDirectory(prefix="comet-upstream-sync-") as temp_dir:
        body_path = Path(temp_dir) / "pr-body.md"
        body_path.write_text(body, encoding="utf-8", newline="\n")
        result = gh.run(
            "pr",
            "create",
            "--draft",
            "--base",
            run.target_branch,
            "--head",
            run.sync_branch,
            "--title",
            title,
            "--body-file",
            str(body_path),
        )
    url = result.stdout.strip()
    if not url:
        raise SyncError("GitHub CLI did not return the created PR URL.")
    return url


def _current_branch(git: Git) -> str:
    try:
        return git.run("symbolic-ref", "--quiet", "--short", "HEAD").stdout.strip()
    except GitError as error:
        raise SyncError("The repository is in detached HEAD state.") from error


def _current_head(git: Git) -> str:
    head = git.run("rev-parse", "HEAD").stdout.strip()
    return _require_sha(head, "Git HEAD")


def _head_state(git: Git) -> HeadState:
    lines = git.run("log", "-1", "--format=%H%n%P%n%s", "HEAD").stdout.splitlines()
    if len(lines) < 3:
        raise SyncError("Git returned an unreadable HEAD description.")
    return HeadState(
        oid=_require_sha(lines[0].strip(), "Git HEAD"),
        parents=tuple(
            _require_sha(parent, "Git HEAD parent") for parent in lines[1].split()
        ),
        subject=lines[2].strip(),
    )


def _print_run_summary(
    selected: list[Commit],
    classifications: list[RunDecision],
    output: TextIO,
) -> None:
    print("Run summary:", file=output)
    print("Implemented by cherry-pick:", file=output)
    if selected:
        for commit in selected:
            print(format_commit(None, commit), file=output)
    else:
        print("(none)", file=output)
    for outcome, heading in (
        ("not-applicable", "Not applicable:"),
        ("deferred", "Deferred:"),
    ):
        print(heading, file=output)
        matching = [
            decision
            for decision in classifications
            if decision.outcome == outcome
        ]
        if not matching:
            print("(none)", file=output)
        for decision in matching:
            print(
                f"{decision.upstream_sha[:7]}  {decision.subject} — "
                f"{decision.note}",
                file=output,
            )


def _ledger_git_path(git: Git, ledger_path: Path) -> str:
    root = git.cwd or Path.cwd()
    try:
        return ledger_path.relative_to(root).as_posix()
    except ValueError as error:
        raise SyncError("The upstream sync ledger is outside the repository.") from error


def _only_ledger_status(status: str, ledger_git_path: str) -> bool:
    lines = [line for line in status.splitlines() if line]
    return bool(lines) and all(
        len(line) >= 4 and line[3:] == ledger_git_path for line in lines
    )


def start_workflow(
    git: Git,
    gh: Gh,
    input_fn: Callable[[str], str],
    output: TextIO,
    day: date,
    ledger_path: Path,
) -> int:
    pending_path = pending_state_path(git)
    if pending_path.exists():
        raise SyncError(
            "A pending upstream sync already exists. Run this command with --resume."
        )
    target = validate_repository(git)
    ensure_upstream(git)
    validate_origin(git)
    validate_github(gh)
    git.run("fetch", "--prune", "upstream", "main")
    ledger = load_ledger(ledger_path)
    commits = filter_resolved(discover_commits(git, target), ledger)
    if not commits:
        print("No unresolved upstream commits.", file=output)
        return 0

    print("Eligible upstream commits (newest first):", file=output)
    for index, commit in enumerate(commits, start=1):
        print(format_commit(index, commit), file=output)
    while True:
        try:
            selected = parse_selection(
                input_fn(
                    "Select commits to cherry-pick "
                    "(for example 1,3-5; blank for none): "
                ),
                commits,
                allow_empty=True,
            )
            break
        except ValueError as error:
            print(error, file=output)
    classifications = classify_unselected(
        commits,
        {commit.oid for commit in selected},
        input_fn,
        output,
    )
    reserved_branches = existing_branches(git) | {
        run.run_id for run in ledger.runs if run.kind == "sync"
    }
    branch = next_branch_name(reserved_branches, day)
    _print_run_summary(selected, classifications, output)
    confirmation = input_fn(
        "Create the sync branch, record this run, push, and open a draft PR? [y/N] "
    )
    if confirmation.strip().lower() not in {"y", "yes"}:
        return 0

    pending = PendingRun(
        1,
        "prepared",
        target,
        branch,
        branch,
        day.isoformat(),
        tuple(selected),
        tuple(classifications),
        {},
        considered=tuple(commits),
    )
    write_pending(pending_path, pending)
    return resume_workflow(git, gh, output, ledger_path, pending_path)


def resume_workflow(
    git: Git,
    gh: Gh,
    output: TextIO,
    ledger_path: Path,
    pending_path: Path,
) -> int:
    if not pending_path.exists():
        raise SyncError("No pending upstream sync exists to resume.")
    pending = load_pending(pending_path)
    validate_origin(git)
    validate_github(gh)
    current = _current_branch(git)

    if pending.phase == "prepared":
        branches = existing_branches(git)
        if current == pending.target_branch and pending.sync_branch not in branches:
            git.run("switch", "-c", pending.sync_branch)
        elif current != pending.sync_branch:
            raise SyncError(
                f"Prepared sync expects {pending.target_branch} or "
                f"{pending.sync_branch}, but {current} is checked out."
            )
        pending = replace(pending, phase="cherry-picking")
        write_pending(pending_path, pending)
        current = pending.sync_branch

    if current != pending.sync_branch:
        raise SyncError(
            f"Pending sync requires branch {pending.sync_branch}, "
            f"but {current} is checked out."
        )

    if pending.phase == "cherry-picking":
        ensure_cherry_pick_resolved(git)
        status = git.run("status", "--porcelain").stdout
        picks_complete = (
            pending.active_upstream_sha is None
            and len(pending.local_commits) == len(pending.selected)
        )
        if status and not picks_complete:
            raise SyncError("The sync branch must have a clean worktree to resume.")
        if pending.active_upstream_sha is not None:
            try:
                pending = verify_resumed_pick(pending, _head_state(git))
            except SyncCancelled:
                clear_pending(pending_path)
                print(
                    f"Upstream sync run cancelled. The branch "
                    f"{pending.sync_branch} was left intact; switch back to "
                    f"{pending.target_branch} when ready.",
                    file=output,
                )
                return 1
            write_pending(pending_path, pending)

        while len(pending.local_commits) < len(pending.selected):
            commit = pending.selected[len(pending.local_commits)]
            pending = record_pick_start(pending, commit, _current_head(git))
            write_pending(pending_path, pending)
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
                print("Resolve or abort the cherry-pick, then choose:", file=output)
                print("git add <resolved-files>", file=output)
                print("git cherry-pick --continue", file=output)
                print("git cherry-pick --abort", file=output)
                print("python scripts/sync-upstream.py --resume", file=output)
                return 1
            pending = record_pick_success(pending, _current_head(git))
            write_pending(pending_path, pending)

    return finish_workflow(
        git, gh, output, ledger_path, pending_path, pending
    )


def finish_workflow(
    git: Git,
    gh: Gh,
    output: TextIO,
    ledger_path: Path,
    pending_path: Path,
    pending: PendingRun,
) -> int:
    run = build_sync_run(
        date.fromisoformat(pending.run_date),
        pending.target_branch,
        pending.sync_branch,
        list(pending.considered),
        pending.local_commits,
        list(pending.classifications),
    )
    if pending.phase == "cherry-picking":
        ledger = load_ledger(ledger_path)
        ledger_git_path = _ledger_git_path(git, ledger_path)
        status = git.run("status", "--porcelain").stdout
        existing_run = next(
            (
                existing
                for existing in ledger.runs
                if existing.run_id == run.run_id
            ),
            None,
        )
        if existing_run is not None:
            if existing_run != run:
                raise SyncError(
                    f"Ledger run {run.run_id} does not match pending state."
                )
            if status and not _only_ledger_status(status, ledger_git_path):
                raise SyncError(
                    "Only the expected upstream sync ledger may be modified."
                )
            needs_commit = bool(status)
        else:
            if status:
                raise SyncError(
                    "The sync branch must have a clean worktree before "
                    "recording the ledger."
                )
            write_ledger(ledger_path, apply_run(ledger, run))
            needs_commit = True
        if needs_commit:
            git.run("add", "--", ledger_git_path)
            git.run(
                "commit",
                "-m",
                f"chore: record upstream sync {pending.run_id}",
            )
        pending = replace(pending, phase="ledger-committed")
        write_pending(pending_path, pending)
    if pending.phase == "ledger-committed":
        git.run("push", "--set-upstream", "origin", pending.sync_branch)
        pending = replace(pending, phase="pushed")
        write_pending(pending_path, pending)
    if pending.phase != "pushed":
        raise SyncError(f"Cannot finish pending phase {pending.phase}.")
    title, body = format_pr(run)
    url = find_existing_pr(
        gh, pending.sync_branch, pending.target_branch
    )
    if url is None:
        url = create_draft_pr(gh, run, title, body)
    clear_pending(pending_path)
    print(f"Draft pull request: {url}", file=output)
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


def force_utf8_console(*streams: TextIO) -> None:
    """Make console output survive non-ASCII commit subjects.

    Upstream subjects carry arrows, em dashes and accented author names, and a
    Windows console hands Python a cp1252 stream by default: the commit listing
    then dies mid-print with UnicodeEncodeError, aborting the run before any
    selection is possible. `errors="replace"` keeps a stream that cannot be
    reconfigured to UTF-8 from taking the run down with it.
    """
    for stream in streams:
        reconfigure = getattr(stream, "reconfigure", None)
        if reconfigure is None:
            continue
        try:
            reconfigure(encoding="utf-8", errors="replace")
        except (OSError, ValueError):
            pass


def main(argv: Sequence[str] | None = None) -> int:
    force_utf8_console(sys.stdout, sys.stderr)
    parser = argparse.ArgumentParser(
        description="Select and cherry-pick commits from zeronsh/comet",
        epilog=(
            "Run from a clean worktree with an attached branch. The helper "
            "configures the fixed upstream remote when missing and refuses a "
            "collision when that name points elsewhere. A configured origin "
            "and authenticated GitHub CLI (`gh auth login`) are required.\n"
            "Choose commits with 2, 1,4, or 2-5, or leave selection blank for "
            "a bookkeeping-only run. Unselected commits are deferred by "
            "default or can be marked not applicable. Review the outcomes and "
            "give confirmation before any branch is created.\n"
            "The helper creates a sync/upstream-YYYY-MM-DD safety branch, "
            "records outcomes in the committed ledger, pushes the branch to "
            "origin, and opens a draft pull request. Bookkeeping-only runs "
            "also open a draft pull request.\n"
            "After resolving or aborting a conflict, or after a push or PR "
            "failure, run with --resume. The helper never merges, force-pushes, "
            "deletes branches, or overwrites remotes."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--resume",
        action="store_true",
        help="resume a pending cherry-pick, push, or draft PR operation",
    )
    args = parser.parse_args(argv)

    try:
        git = Git()
        root = repository_root(git)
        git.cwd = root
        gh = Gh(root)
        ledger_path = root / ".github" / "upstream-sync.json"
        if args.resume:
            return resume_workflow(
                git,
                gh,
                sys.stdout,
                ledger_path,
                pending_state_path(git),
            )
        return start_workflow(
            git,
            gh,
            input,
            sys.stdout,
            date.today(),
            ledger_path,
        )
    except GitError as error:
        command = "git " + " ".join(error.args_list)
        detail = format_failure_detail(
            error.stdout, error.stderr, error.returncode
        )
        print(f"error: {command} failed: {detail}", file=sys.stderr)
        return 1
    except GhError as error:
        command = "gh " + " ".join(error.args_list)
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
