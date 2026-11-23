"""Validate proof of merges and builds, and update the corresponding retirement state."""

from urllib.parse import urlsplit

from epic_retirement_records import (
    RetirementError,
    artifact,
    fields,
    nonempty,
    require,
    sha,
    uuid,
)


def evidence_data(value):
    require(isinstance(value, dict), "Expected an evidence object")
    kind = value.get("kind")
    common = ("kind", "approval_revision", "evidence")
    if kind == "merge":
        data = merge_details(value, common)
    elif kind == "ordinary":
        data = ordinary_details(value, common)
    elif kind == "integrated":
        data = integrated_details(value, common)
    else:
        raise RetirementError("Expected merge, ordinary or integrated evidence")
    if kind != "merge":
        require(type(value["passed"]) is bool, "passed must be boolean")
        data["passed"] = value["passed"]

    return kind, data


def merge_details(value, common):
    fields(
        value,
        (*common, "repository", "pull_request", "sha"),
        ("supersedes_merge",),
    )
    url = urlsplit(nonempty(value["pull_request"], 2048))
    require(
        url.scheme == "https" and url.hostname and not (url.username or url.password),
        "Supply an HTTPS pull request URL",
    )
    data = {
        "repository": nonempty(value["repository"], 96),
        "pull_request": value["pull_request"],
        "sha": sha(value["sha"]),
    }
    if "supersedes_merge" in value:
        data["supersedes_merge"] = uuid(value["supersedes_merge"])
    return data


def ordinary_details(value, common):
    fields(value, (*common, "repository", "sha", "passed", "flag_absent"))
    require(type(value["flag_absent"]) is bool, "flag_absent must be boolean")
    return {
        "repository": nonempty(value["repository"], 96),
        "sha": sha(value["sha"]),
        "flag_absent": value["flag_absent"],
    }


def integrated_details(value, common):
    fields(value, (*common, "revisions", "builds", "passed"))
    require(
        isinstance(value["revisions"], dict) and isinstance(value["builds"], dict),
        "Supply a revision and ordinary build artifact for each repository",
    )
    return {
        "revisions": {repo: sha(commit) for repo, commit in value["revisions"].items()},
        "builds": {repo: artifact(build) for repo, build in value["builds"].items()},
    }


def apply_evidence(state, current, event, anchor):
    merges, ordinary = current["merges"], current["ordinary"]
    kind = event["kind"]
    if kind in ("merge", "ordinary"):
        repo = event["repository"]
        require(repo in state["repositories"], f"{repo} is not registered")
    if kind == "merge":
        require(
            state["merges"].get(repo) == event.get("supersedes_merge"),
            f"Merge for {repo} must supersede its current receipt from status",
        )
        require(
            repo not in merges or merges[repo]["sha"] != event["sha"],
            f"A repair merge for {repo} must have a different SHA",
        )
        state = {
            **state,
            "merges": {**state["merges"], repo: anchor},
            "ordinary": {
                key: value for key, value in state["ordinary"].items() if key != repo
            },
            "integrated": None,
        }
        commits = {repo: event["sha"]}
    elif kind == "ordinary":
        require(
            repo in merges and merges[repo]["sha"] == event["sha"],
            f"Ordinary build for {repo} must match its recorded merge SHA",
        )
        state = {**state, "ordinary": {**state["ordinary"], repo: anchor}}
        commits = {repo: event["sha"]}
    else:
        commits = validate_integrated(state, event, merges, ordinary)
        state = {**state, "integrated": anchor}
    return state, commits


def merge_shas(merges):
    return {repo: item["sha"] for repo, item in merges.items()}


def passing_ordinary_build(build):
    return build["passed"] and build["flag_absent"]


def validate_integrated(state, data, merges, ordinary):
    commits = merge_shas(merges)
    require(
        data["revisions"] == commits and commits.keys() == state["repositories"].keys(),
        "Integrated check must name all recorded cleanup merge SHAs",
    )
    builds = {
        repo: item["evidence"]
        for repo, item in ordinary.items()
        if passing_ordinary_build(item)
    }
    require(
        data["builds"] == builds and builds.keys() == commits.keys(),
        "Integrated check must name the passing flag-free ordinary builds",
    )
    return commits


def missing_builds(state, evidence):
    merges, ordinary = (evidence[key] for key in ("merges", "ordinary"))
    for repo in sorted(state["repositories"]):
        if repo not in merges:
            yield f"{repo}: cleanup merge pending"
            continue
        build = ordinary.get(repo)
        if not build or build["sha"] != merges[repo]["sha"]:
            yield f"{repo}: ordinary build evidence pending"
        elif not passing_ordinary_build(build):
            yield f"{repo}: ordinary build or flag removal check failed"


def missing_acceptance(evidence):
    merges, ordinary, integrated = (
        evidence[key] for key in ("merges", "ordinary", "integrated")
    )
    if not integrated:
        yield "Integrated acceptance pending"
    elif not integrated["passed"]:
        yield "Integrated acceptance failed"
    elif integrated["revisions"] != merge_shas(merges) or integrated["builds"] != {
        repo: item["evidence"] for repo, item in ordinary.items()
    }:
        yield "Integrated acceptance evidence no longer matches the ordinary builds"
