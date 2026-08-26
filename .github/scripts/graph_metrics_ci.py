#!/usr/bin/env python3
import argparse
import collections
import fnmatch
import hashlib
import json
import os
import platform
import subprocess
import sys
from pathlib import Path

sys.setrecursionlimit(10000)

def canonical_bytes(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")

def digest_bytes(value):
    return hashlib.sha256(value).hexdigest()

def digest_file(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()

def load_json(path):
    return json.loads(Path(path).read_text(encoding="utf-8"))

def write_json(path, value):
    Path(path).write_text(
        json.dumps(value, sort_keys=True, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )

def seal(value):
    result = dict(value)
    result["artifact_digest"] = digest_bytes(canonical_bytes(value))
    return result

def relative_path(value, repo):
    text = str(value or "").replace("\\", "/")
    root = str(Path(repo).resolve()).replace("\\", "/").rstrip("/")
    if text == root:
        return ""
    if text.startswith(root + "/"):
        text = text[len(root) + 1:]
    while text.startswith("./"):
        text = text[2:]
    if not text or text.startswith("/") or text == ".." or text.startswith("../"):
        return None
    return text

def boundary_for(path, boundaries):
    for boundary in boundaries:
        for prefix in boundary["prefixes"]:
            if path == prefix.rstrip("/") or path.startswith(prefix):
                return boundary["name"]
    return "other"

def tracked_source_files(repo, policy):
    git_environment = {
        "PATH": "/usr/bin:/bin",
        "HOME": os.environ.get("HOME", "/tmp"),
        "LC_ALL": "C",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_NO_REPLACE_OBJECTS": "1",
        "GIT_ALLOW_PROTOCOL": "file",
    }
    raw = subprocess.check_output(
        ["/usr/bin/git", "-C", str(repo), "ls-files", "-z"],
        env=git_environment,
    )
    corpus = policy["corpus"]
    suffixes = tuple(suffix.lower() for suffix in corpus["source_suffixes"] + corpus["template_suffixes"])
    result = []
    for entry in raw.decode("utf-8", errors="surrogateescape").split("\0"):
        if not entry:
            continue
        normalized = entry.replace("\\", "/")
        if not any(fnmatch.fnmatchcase(normalized, pattern) for pattern in corpus["include"]):
            continue
        if any(fnmatch.fnmatchcase(normalized, pattern) for pattern in corpus["exclude"]):
            continue
        if normalized.lower().endswith(suffixes):
            result.append(normalized)
    return sorted(set(result))

def scc_state(nodes, edges):
    adjacency = {node: set() for node in nodes}
    for source, target in edges:
        adjacency.setdefault(source, set()).add(target)
        adjacency.setdefault(target, set())
    index = 0
    stack = []
    on_stack = set()
    indices = {}
    low = {}
    components = []

    def visit(node):
        nonlocal index
        indices[node] = index
        low[node] = index
        index += 1
        stack.append(node)
        on_stack.add(node)
        for target in sorted(adjacency[node]):
            if target not in indices:
                visit(target)
                low[node] = min(low[node], low[target])
            elif target in on_stack:
                low[node] = min(low[node], indices[target])
        if low[node] == indices[node]:
            members = []
            while True:
                member = stack.pop()
                on_stack.remove(member)
                members.append(member)
                if member == node:
                    break
            components.append(tuple(sorted(members)))

    for node in sorted(adjacency):
        if node not in indices:
            visit(node)
    components.sort()
    self_loops = {source for source, target in edges if source == target}
    cyclic = [component for component in components if len(component) > 1 or component[0] in self_loops]
    component_for = {}
    cyclic_component_for = {}
    for component in components:
        identifier = digest_bytes(canonical_bytes(list(component)))
        for member in component:
            component_for[member] = identifier
        if component in cyclic:
            for member in component:
                cyclic_component_for[member] = identifier
    return components, cyclic, component_for, cyclic_component_for

def projection(graph, repo, policy, kinds, in_scope):
    node_files = collections.defaultdict(set)
    parsed = set()
    for node in graph.get("nodes", []):
        path = relative_path(node.get("file_path"), repo)
        qualified = node.get("qualified_name")
        if path in in_scope and isinstance(qualified, str):
            node_files[qualified].add(path)
            if node.get("kind") == "File":
                parsed.add(path)

    def resolve_entity(qualified):
        candidates = node_files.get(qualified, set())
        if len(candidates) == 1:
            candidate = next(iter(candidates))
            return candidate if candidate in parsed else None
        if len(candidates) > 1:
            return None
        prefix = str(qualified or "").split("::", 1)[0]
        fallback = relative_path(prefix, repo)
        return fallback if fallback in parsed else None

    edges = set()
    edge_kind_counts = collections.Counter()
    confidence_tier_counts = collections.Counter()
    unresolved_edges_by_kind = collections.Counter()
    multiplicity_count = 0
    for edge in graph.get("edges", []):
        kind = str(edge.get("kind", ""))
        if kind not in kinds:
            continue
        source = resolve_entity(edge.get("source"))
        target = resolve_entity(edge.get("target"))
        if source is None or target is None:
            unresolved_edges_by_kind[kind] += 1
            continue
        edges.add((source, target))
        multiplicity_count += 1
        edge_kind_counts[kind] += 1
        confidence_tier_counts[str(edge.get("confidence_tier", "UNKNOWN"))] += 1
    components, cyclic, _component_for, cyclic_component_for = scc_state(parsed, edges)
    cyclic_nodes = {member for component in cyclic for member in component}
    indegree = collections.Counter()
    outdegree = collections.Counter()
    for source, target in edges:
        outdegree[source] += 1
        indegree[target] += 1
    boundaries = policy["boundaries"]
    scc_records = []
    excess = 0
    cross_boundary = 0
    for component in cyclic:
        members = set(component)
        internal = [edge for edge in edges if edge[0] in members and edge[1] in members]
        component_excess = max(0, len(internal) - len(component))
        component_cross = sum(
            1 for source, target in internal
            if boundary_for(source, boundaries) != boundary_for(target, boundaries)
        )
        excess += component_excess
        cross_boundary += component_cross
        scc_records.append({
            "scc_id": digest_bytes(canonical_bytes(list(component))),
            "members": list(component),
            "node_count": len(component),
            "internal_distinct_edges": len(internal),
            "excess_cyclic_edges": component_excess,
            "cross_boundary_cyclic_edges": component_cross,
        })
    scc_records.sort(key=lambda item: (-item["node_count"], item["scc_id"]))

    def top(counter):
        return [
            {"path": path, "count": count}
            for path, count in sorted(counter.items(), key=lambda item: (-item[1], item[0]))[:20]
        ]

    public = {
        "nodes": len(parsed),
        "distinct_directed_edges": len(edges),
        "edge_multiplicity_count": multiplicity_count,
        "edge_kinds": dict(sorted(edge_kind_counts.items())),
        "confidence_tiers": dict(sorted(confidence_tier_counts.items())),
        "unresolved_edges_by_kind": dict(sorted(unresolved_edges_by_kind.items())),
        "ambiguous_entity_count": sum(1 for paths in node_files.values() if len(paths) > 1),
        "self_loops": sum(1 for source, target in edges if source == target),
        "scc_count": len(components),
        "cyclic_scc_count": len(cyclic),
        "largest_scc_nodes": max((len(component) for component in cyclic), default=0),
        "cyclic_nodes": len(cyclic_nodes),
        "cyclic_node_proportion": round(len(cyclic_nodes) / len(parsed), 8) if parsed else 0.0,
        "cycle_mass": len(cyclic_nodes),
        "excess_cyclic_edges": excess,
        "cross_boundary_cyclic_edges": cross_boundary,
        "fan_in_top": top(indegree),
        "fan_out_top": top(outdegree),
        "cyclic_sccs": scc_records,
    }
    return {
        "public": public,
        "nodes": parsed,
        "edges": edges,
        "cyclic": cyclic,
        "cyclic_component_for": cyclic_component_for,
        "indegree": indegree,
        "outdegree": outdegree,
    }

def coverage(graph, repo, policy, tracked):
    parsed = set()
    for node in graph.get("nodes", []):
        path = relative_path(node.get("file_path"), repo)
        if path is not None and node.get("kind") == "File":
            parsed.add(path)
    tracked_set = set(tracked)
    measured = sorted(tracked_set & parsed)
    unsupported = sorted(tracked_set - parsed)

    def bytes_for(paths):
        total = 0
        for path in paths:
            candidate = Path(repo) / path
            try:
                total += candidate.lstat().st_size
            except OSError:
                pass
        return total

    total_bytes = bytes_for(tracked)
    parsed_bytes = bytes_for(measured)
    return {
        "declared_source_files": len(tracked),
        "parsed_files": len(measured),
        "parser_coverage": round(len(measured) / len(tracked), 8) if tracked else 1.0,
        "declared_source_bytes": total_bytes,
        "parsed_bytes": parsed_bytes,
        "parser_coverage_by_bytes": round(parsed_bytes / total_bytes, 8) if total_bytes else 1.0,
        "unsupported_files": unsupported,
    }

def sanitized_crg(graph, status, risk, policy):
    raw_risk = None
    if isinstance(risk, dict):
        raw_risk = {
            "status": "measured",
            "risk_score": risk.get("risk_score"),
            "changed_function_count": len(risk.get("changed_functions", [])),
            "affected_flow_count": len(risk.get("affected_flows", [])),
            "test_gap_count": len(risk.get("test_gaps", [])),
            "functions_truncated": bool(risk.get("functions_truncated", False)),
        }
    else:
        raw_risk = {"status": "unavailable"}
    stats = graph.get("stats", {})
    return {
        "analyzer": policy["analyzer"]["name"],
        "version": policy["analyzer"]["version"],
        "schema_version": policy["analyzer"]["schema_version"],
        "nodes": status.get("nodes"),
        "edges": status.get("edges"),
        "files": status.get("files"),
        "languages": sorted(status.get("languages", [])),
        "nodes_by_kind": dict(sorted(stats.get("nodes_by_kind", {}).items())),
        "edges_by_kind": dict(sorted(stats.get("edges_by_kind", {}).items())),
        "risk": raw_risk,
        "risk_is_heuristic": True,
    }

def compare_projection(base, head):
    added = sorted(head["edges"] - base["edges"])
    removed = sorted(base["edges"] - head["edges"])
    new_cycle = []
    for edge in added:
        source, target = edge
        head_component = head["cyclic_component_for"].get(source)
        base_source_component = base["cyclic_component_for"].get(source)
        base_target_component = base["cyclic_component_for"].get(target)
        if head_component and head_component == head["cyclic_component_for"].get(target) and (
            not base_source_component or base_source_component != base_target_component
        ):
            new_cycle.append({"source": source, "target": target})
    persistence = []
    head_cyclic = list(head["cyclic"])
    for component in base["cyclic"]:
        base_set = set(component)
        candidates = []
        for candidate in head_cyclic:
            candidate_set = set(candidate)
            union = base_set | candidate_set
            overlap = len(base_set & candidate_set) / len(union) if union else 0.0
            candidates.append((overlap, digest_bytes(canonical_bytes(list(candidate))), candidate))
        candidates.sort(key=lambda item: (-item[0], item[1]))
        best = candidates[0] if candidates else (0.0, None, ())
        if best[0] == 0:
            best = (0.0, None, ())
        persistence.append({
            "base_scc_id": digest_bytes(canonical_bytes(list(component))),
            "head_scc_id": best[1],
            "jaccard": round(best[0], 8),
            "retained": sorted(base_set & set(best[2])),
            "exited": sorted(base_set - set(best[2])),
            "entered": sorted(set(best[2]) - base_set),
        })
    persistence.sort(key=lambda item: item["base_scc_id"])
    paths = sorted(base["nodes"] | head["nodes"])
    fan_changes = []
    for path in paths:
        fan_changes.append({
            "path": path,
            "fan_in_delta": head["indegree"].get(path, 0) - base["indegree"].get(path, 0),
            "fan_out_delta": head["outdegree"].get(path, 0) - base["outdegree"].get(path, 0),
        })
    fan_changes.sort(
        key=lambda item: (
            -max(abs(item["fan_in_delta"]), abs(item["fan_out_delta"])),
            item["path"],
        )
    )
    numeric = [
        "nodes", "distinct_directed_edges", "edge_multiplicity_count", "self_loops",
        "scc_count", "cyclic_scc_count", "largest_scc_nodes", "cyclic_nodes",
        "cyclic_node_proportion", "cycle_mass", "excess_cyclic_edges",
        "cross_boundary_cyclic_edges",
    ]
    return {
        "delta": {
            key: round(head["public"][key] - base["public"][key], 8)
            for key in numeric
        },
        "added_edges": [list(edge) for edge in added],
        "removed_edges": [list(edge) for edge in removed],
        "new_cycle_forming_edges": new_cycle,
        "scc_persistence": persistence,
        "fan_changes_top": fan_changes[:20],
    }

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--raw", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--base-repo", required=True)
    parser.add_argument("--head-repo", required=True)
    parser.add_argument("--base", required=True)
    parser.add_argument("--head", required=True)
    parser.add_argument("--policy", required=True)
    parser.add_argument("--behavior-policy", required=True)
    parser.add_argument("--dependency-lock", required=True)
    parser.add_argument("--analyzer-identity", required=True)
    parser.add_argument("--evaluator", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--repository-id", required=True)
    parser.add_argument("--head-repository-id", required=True)
    parser.add_argument("--event-name", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--run-attempt", required=True)
    parser.add_argument("--pr-number", default="")
    parser.add_argument("--configuration-changed", choices=["true", "false"], required=True)
    args = parser.parse_args()

    raw = Path(args.raw)
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=False)
    policy = load_json(args.policy)
    behavior_policy = load_json(args.behavior_policy)
    analyzer_identity = load_json(args.analyzer_identity)
    base_graph = load_json(raw / "base-graph.json")
    head_graph = load_json(raw / "head-graph.json")
    base_status = load_json(raw / "base-status.json")
    head_status = load_json(raw / "head-status.json")
    try:
        head_risk = load_json(raw / "head-risk.json")
    except (OSError, json.JSONDecodeError):
        head_risk = None

    corpus_contract = {
        "include": policy["corpus"]["include"],
        "exclude": policy["corpus"]["exclude"],
        "source_suffixes": policy["corpus"]["source_suffixes"],
        "template_suffixes": policy["corpus"]["template_suffixes"],
    }
    repository_root = Path(args.policy).resolve().parent.parent
    workflow_paths = (
        ".github/workflows/graph-metrics.yml",
        ".github/workflows/graph-metrics-publish.yml",
    )
    workflow_files = {}
    for relative in workflow_paths:
        candidate = repository_root / relative
        if not candidate.is_file() or candidate.is_symlink():
            raise SystemExit(f"unsafe workflow configuration file: {relative}")
        workflow_files[relative] = digest_file(candidate)
    configuration_contract = {
        "schema_version": policy["ci_adapter"]["configuration_schema"],
        "files": workflow_files,
    }
    schemas = {
        "policy": policy["schema_version"],
        "snapshot": policy["ci_adapter"]["snapshot_schema"],
        "comparison": policy["ci_adapter"]["comparison_schema"],
        "behavior": policy["ci_adapter"]["behavior_schema"],
        "delphi": policy["ci_adapter"]["delphi_schema"],
        "artifact": policy["publication"]["artifact_schema"],
    }
    git_executable = Path("/usr/bin/git").resolve()
    git_environment = {
        "PATH": "/usr/bin:/bin",
        "HOME": os.environ.get("HOME", "/tmp"),
        "LC_ALL": "C",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_NO_REPLACE_OBJECTS": "1",
    }
    git_version = subprocess.check_output(
        [str(git_executable), "--version"], env=git_environment, text=True
    ).strip()
    actual_environment = {
        "os": platform.system(),
        "release": platform.release(),
        "machine": platform.machine(),
        "python": platform.python_version(),
        "python_implementation": platform.python_implementation(),
        "runner_image_os": os.environ.get("ImageOS"),
        "runner_image_version": os.environ.get("ImageVersion"),
        "runner_arch": os.environ.get("RUNNER_ARCH"),
        "declared_runner": policy["runtime"]["os"],
        "CRG_SERIAL_PARSE": os.environ.get("CRG_SERIAL_PARSE"),
        "CRG_LEIDEN_SEED": os.environ.get("CRG_LEIDEN_SEED"),
        "PYTHONHASHSEED": os.environ.get("PYTHONHASHSEED"),
        "TZ": os.environ.get("TZ"),
        "git": {
            "version": git_version,
            "executable_sha256": digest_file(git_executable),
            "config_nosystem": "1",
            "config_global": "/dev/null",
            "no_replace_objects": "1",
            "allow_protocol": "file",
        },
        "analyzer": {
            "version": analyzer_identity["version"],
            "installed_code_sha256": analyzer_identity["installed_code_sha256"],
            "entrypoint_sha256": analyzer_identity["entrypoint_sha256"],
            "interpreter_sha256": analyzer_identity["interpreter_sha256"],
        },
        "crg_invocation": {
            "build": ["--skip-flows", "--skip-postprocess", "--quiet"],
            "status": ["--json"],
            "visualize": ["--format", "json"],
            "detect_changes": ["--base", "<base-sha>"],
            "network_proxies": "loopback-deny",
        },
    }
    digests = {
        "analyzer": analyzer_identity["installed_code_sha256"],
        "dependency_lock": digest_file(args.dependency_lock),
        "policy": digest_bytes(canonical_bytes(policy)),
        "behavior_policy": digest_bytes(canonical_bytes(behavior_policy)),
        "schema": digest_bytes(canonical_bytes(schemas)),
        "normalizer": digest_bytes(canonical_bytes(policy["ci_adapter"]["normalizer"])),
        "corpus": digest_bytes(canonical_bytes(corpus_contract)),
        "harness": behavior_policy["harness"]["digest"],
        "environment": digest_bytes(canonical_bytes(actual_environment)),
        "runtime": digest_bytes(canonical_bytes({
            "actual_environment": actual_environment,
            "entrypoint_sha256": analyzer_identity["entrypoint_sha256"],
            "interpreter_sha256": analyzer_identity["interpreter_sha256"],
        })),
        "evaluator": digest_file(args.evaluator),
        "configuration": digest_bytes(canonical_bytes(configuration_contract)),
    }
    configuration_changed = args.configuration_changed == "true"
    comparable = not configuration_changed
    comparability = {
        "status": "comparable" if comparable else "not-comparable",
        "reason": None if comparable else policy["comparability"]["configuration_change_result"],
        "required_matching_digests": policy["comparability"]["required_matching_digests"],
        "configuration_changed": configuration_changed,
    }

    projection_states = {}
    base_public = {}
    head_public = {}
    comparison_public = {}
    base_tracked = tracked_source_files(args.base_repo, policy)
    head_tracked = tracked_source_files(args.head_repo, policy)
    base_scope = set(base_tracked)
    head_scope = set(head_tracked)
    for name, kinds in sorted(policy["projections"].items()):
        base_state = projection(
            base_graph, args.base_repo, policy, set(kinds), base_scope
        )
        head_state = projection(
            head_graph, args.head_repo, policy, set(kinds), head_scope
        )
        projection_states[name] = (base_state, head_state)
        base_public[name] = base_state["public"]
        head_public[name] = head_state["public"]
        comparison_public[name] = compare_projection(base_state, head_state) if comparable else {
            "status": "not-comparable",
            "reason": policy["comparability"]["configuration_change_result"],
        }

    base_coverage = coverage(base_graph, args.base_repo, policy, base_tracked)
    head_coverage = coverage(head_graph, args.head_repo, policy, head_tracked)
    base_crg = sanitized_crg(base_graph, base_status, None, policy)
    head_crg = sanitized_crg(head_graph, head_status, head_risk, policy)
    limitations = [
        "CI uses a bounded file-level topology adapter; the full portable evaluator remains external.",
        "CRG extraction and resolution are heuristic even when topology calculations over exported edges are deterministic.",
        "Behavior is inconclusive because this parser job does not build or execute git-ai or its frozen black-box harness.",
        "History and Delphi rounds are unavailable in CI and run interactively through the portable skill.",
    ]

    snapshot = seal({
        "schema_version": schemas["snapshot"],
        "artifact_type": "snapshot",
        "adapter_scope": policy["ci_adapter"]["scope"],
        "revision": args.head,
        "digests": digests,
        "parser": head_coverage,
        "topology": {"projections": head_public},
        "crg": head_crg,
        "behavior": {
            "change_intent": behavior_policy["change_intent"],
            "evidence_result": "inconclusive",
            "executed_harness_digest": None,
        },
        "delphi": {"status": "unavailable"},
        "limitations": limitations,
    })
    comparison = seal({
        "schema_version": schemas["comparison"],
        "artifact_type": "comparison",
        "adapter_scope": policy["ci_adapter"]["scope"],
        "base": args.base,
        "head": args.head,
        "digests": digests,
        "comparability": comparability,
        "parser": {
            "base": base_coverage,
            "head": head_coverage,
            "coverage_delta": (
                round(head_coverage["parser_coverage"] - base_coverage["parser_coverage"], 8)
                if comparable else None
            ),
            "comparison_status": "comparable" if comparable else "not-comparable",
            "comparison_reason": None if comparable else policy["comparability"]["configuration_change_result"],
        },
        "topology": {
            "base": base_public,
            "head": head_public,
            "comparison": comparison_public,
        },
        "crg": {"base": base_crg, "head": head_crg},
        "behavior": {
            "change_intent": behavior_policy["change_intent"],
            "evidence_result": "inconclusive",
            "executed_harness_digest": None,
        },
        "delphi": {"status": "unavailable"},
        "limitations": limitations,
    })
    behavior = seal({
        "schema_version": schemas["behavior"],
        "artifact_type": "behavior-evidence",
        "base": args.base,
        "head": args.head,
        "digests": digests,
        "behavior": {
            "change_intent": behavior_policy["change_intent"],
            "evidence_result": "inconclusive",
            "historical_label": "unavailable",
            "declared_harness_digest": behavior_policy["harness"]["digest"],
            "executed_harness_digest": None,
            "declared_corpus_digest": behavior_policy["harness"]["corpus"]["canonical_sha256"],
            "executed_corpus_digest": None,
            "declared_normalizer_digest": behavior_policy["harness"]["normalizer"]["sha256"],
            "executed_normalizer_digest": None,
            "matched_cases": [],
            "expected_differences": [],
            "unexpected_differences": [],
            "public_api_additions": [],
            "public_api_removals": [],
            "removed_or_weakened_assertions": behavior_policy["removed_or_weakened_assertions"],
            "unmeasured_surfaces": behavior_policy["unmeasured_surfaces"],
            "blocking_evidence_gaps": behavior_policy["blocking_evidence_gaps"],
            "residual_risk": behavior_policy["residual_risk"],
        },
    })
    delphi = seal({
        "schema_version": schemas["delphi"],
        "artifact_type": "delphi-rounds",
        "digests": digests,
        "delphi": {
            "panel_label": policy["delphi"]["panel_label"],
            "status": "unavailable",
            "blind_status": "not-run",
            "rounds": [],
            "abstentions": [],
            "dissent": [],
            "reason": "CI has no LLM credentials or authorized trusted agent runtime; rounds run interactively.",
        },
    })

    write_json(output / "snapshot.json", snapshot)
    write_json(output / "comparison.json", comparison)
    write_json(output / "behavior-evidence.json", behavior)
    write_json(output / "delphi-rounds.json", delphi)
    (output / "history.ndjson").write_text("", encoding="utf-8")

    imports = head_public.get("imports", {})
    report = "\n".join([
        "# Architecture evidence (report only)",
        "",
        f"- Base: `{args.base}`",
        f"- Head: `{args.head}`",
        f"- Comparability: **{comparability['status']}**" + (
            f" — {comparability['reason']}" if comparability["reason"] else ""
        ),
        f"- Parser coverage: {head_coverage['parsed_files']}/{head_coverage['declared_source_files']} "
        f"({head_coverage['parser_coverage'] * 100:.3f}%)",
        f"- Unsupported files: {len(head_coverage['unsupported_files'])}",
        "",
        "## Machine signal",
        "",
        f"- Imports largest SCC: {imports.get('largest_scc_nodes', 0)}",
        f"- Imports cyclic nodes: {imports.get('cyclic_nodes', 0)}",
        f"- Imports cross-boundary cyclic edges: {imports.get('cross_boundary_cyclic_edges', 0)}",
        f"- CRG heuristic risk: {head_crg['risk'].get('risk_score', 'unavailable')} (separate heuristic lane)",
        "",
        "## Behavioral evidence",
        "",
        f"- Intent: {behavior_policy['change_intent']}",
        "- Result: **inconclusive**",
        "- The CI parser job does not execute the frozen black-box oracle.",
        "",
        "## Delphi-inspired judgment",
        "",
        "- Status: **unavailable in CI**",
        f"- Panel contract: {policy['delphi']['panel_label']}",
        "- Interactive rounds preserve blinding, abstentions, dissent, and minority rationales.",
        "",
        "## Historical study",
        "",
        f"- Candidate frame: {policy['history']['candidate_frame_count']}",
        f"- Primary cases: {policy['history']['primary_case_count']}",
        f"- Discovery: {policy['history']['discovery_signal_count']} signal + "
        f"{policy['history']['discovery_control_count']} controls",
        f"- Holdout: {policy['history']['holdout_signal_count']} signal + "
        f"{policy['history']['holdout_control_count']} controls",
        "- Status: unavailable in CI; generated interactively through the portable skill.",
        "",
        "This report has no composite quality score, merge gate, automatic refactor, or behavioral-equivalence claim.",
        "",
    ])
    (output / "report.md").write_text(report, encoding="utf-8")
    files = {
        name: digest_file(output / name)
        for name in (
            "snapshot.json",
            "comparison.json",
            "history.ndjson",
            "behavior-evidence.json",
            "delphi-rounds.json",
            "report.md",
        )
    }
    metadata = {
        "schema_version": schemas["artifact"],
        "artifact_type": "architecture-evidence-bundle",
        "schema_compatibility": policy["ci_adapter"]["schema_compatibility"],
        "repository": args.repository,
        "repository_id": int(args.repository_id),
        "head_repository_id": int(args.head_repository_id),
        "event_name": args.event_name,
        "run_id": int(args.run_id),
        "run_attempt": int(args.run_attempt),
        "pr_number": int(args.pr_number) if args.pr_number else None,
        "base_sha": args.base,
        "head_sha": args.head,
        "configuration_changed": configuration_changed,
        "comparability": comparability,
        "digests": digests,
        "environment": actual_environment,
        "analyzer_identity": analyzer_identity,
        "history": {"status": "unavailable", "case_count": 0},
        "behavior": {"evidence_result": "inconclusive"},
        "delphi": {"status": "unavailable"},
        "files": files,
    }
    write_json(output / "run-metadata.json", seal(metadata))

if __name__ == "__main__":
    main()
