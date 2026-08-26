#!/usr/bin/env python3
"""Validate hostile architecture evidence and render trusted Markdown.

This program runs only from a trusted default-branch checkout.  It treats the
downloaded workflow artifact as attacker-controlled bytes and never imports or
executes anything from the archive.
"""

import argparse
import hashlib
import json
import math
import os
import re
import stat
import sys
import zipfile
from pathlib import Path, PurePosixPath


SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
REPOSITORY_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
MARKER = "<!-- git-ai-architecture-evidence-v1 -->"
REBASELINE = "not comparable; rebaseline required"
EXPECTED_MEMBERS = frozenset(
    {
        "snapshot.json",
        "comparison.json",
        "history.ndjson",
        "behavior-evidence.json",
        "delphi-rounds.json",
        "report.md",
        "run-metadata.json",
    }
)
JSON_MEMBERS = frozenset(EXPECTED_MEMBERS - {"history.ndjson", "report.md"})


class ValidationError(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise ValidationError(message)


def canonical_bytes(value):
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")


def digest_bytes(value):
    return hashlib.sha256(value).hexdigest()


def digest_file(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def digest_regular_file(path, label):
    candidate = Path(path)
    require(candidate.is_file() and not candidate.is_symlink(), f"{label}: unsafe file")
    return digest_file(candidate)


def reject_constant(value):
    raise ValidationError(f"non-finite JSON number is forbidden: {value}")


def reject_duplicates(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValidationError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def loads_json_strict(raw, label):
    require(raw and not raw.startswith(b"\xef\xbb\xbf"), f"{label}: empty or BOM JSON")
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ValidationError(f"{label}: invalid UTF-8") from error
    try:
        value = json.loads(
            text,
            object_pairs_hook=reject_duplicates,
            parse_constant=reject_constant,
        )
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise ValidationError(f"{label}: invalid JSON") from error
    require(isinstance(value, dict), f"{label}: top level must be an object")
    return value


def load_json_file(path, label):
    candidate = Path(path)
    require(candidate.is_file() and not candidate.is_symlink(), f"{label}: unsafe file")
    return loads_json_strict(candidate.read_bytes(), label)


def exact_keys(value, expected, label):
    require(isinstance(value, dict), f"{label}: expected object")
    actual = set(value)
    expected = set(expected)
    require(
        actual == expected,
        f"{label}: key mismatch (missing={sorted(expected - actual)}, extra={sorted(actual - expected)})",
    )


def required_keys(value, expected, label):
    require(isinstance(value, dict), f"{label}: expected object")
    missing = set(expected) - set(value)
    require(not missing, f"{label}: missing keys {sorted(missing)}")


def sha256(value, label):
    require(isinstance(value, str) and SHA256_RE.fullmatch(value), f"{label}: invalid SHA-256")
    return value


def commit_sha(value, label):
    require(isinstance(value, str) and SHA_RE.fullmatch(value), f"{label}: invalid commit SHA")
    return value


def integer(value, label, minimum=0):
    require(
        isinstance(value, int) and not isinstance(value, bool) and value >= minimum,
        f"{label}: invalid integer",
    )
    return value


def number(value, label, minimum=None, maximum=None):
    require(
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(value),
        f"{label}: invalid number",
    )
    if minimum is not None:
        require(value >= minimum, f"{label}: below minimum")
    if maximum is not None:
        require(value <= maximum, f"{label}: above maximum")
    return value


def string_list(value, label, maximum=10000):
    require(isinstance(value, list) and len(value) <= maximum, f"{label}: invalid list")
    require(all(isinstance(item, str) for item in value), f"{label}: non-string item")
    return value


def validate_self_digest(value, label):
    supplied = sha256(value.get("artifact_digest"), f"{label}.artifact_digest")
    unsigned = dict(value)
    unsigned.pop("artifact_digest", None)
    require(digest_bytes(canonical_bytes(unsigned)) == supplied, f"{label}: self-digest mismatch")


def safe_archive_name(info):
    name = info.filename
    require(isinstance(name, str) and name, "archive: empty member name")
    require("\x00" not in name and "\\" not in name, f"archive: unsafe member name {name!r}")
    path = PurePosixPath(name)
    require(not path.is_absolute(), f"archive: absolute member path {name!r}")
    require(name == path.as_posix(), f"archive: non-canonical member path {name!r}")
    require(all(part not in {"", ".", ".."} for part in path.parts), f"archive: traversal path {name!r}")
    require(len(path.parts) == 1, f"archive: nested member path {name!r}")
    require(not info.is_dir(), f"archive: directory member {name!r}")
    require(not (info.flag_bits & 0x1), f"archive: encrypted member {name!r}")
    mode = (info.external_attr >> 16) & 0xFFFF
    require(not stat.S_ISLNK(mode), f"archive: symlink member {name!r}")
    require(
        info.compress_type in {zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED},
        f"archive: unsupported compression for {name!r}",
    )
    return name


def read_archive(path, compressed_limit, uncompressed_limit):
    archive = Path(path)
    require(archive.is_file() and not archive.is_symlink(), "archive: unsafe input file")
    require(0 < archive.stat().st_size <= compressed_limit, "archive: compressed size outside policy")
    members = {}
    try:
        with zipfile.ZipFile(archive) as bundle:
            infos = bundle.infolist()
            names = [safe_archive_name(info) for info in infos]
            require(len(names) == len(set(names)), "archive: duplicate member name")
            require(set(names) == EXPECTED_MEMBERS, "archive: missing or unexpected members")
            declared_size = sum(info.file_size for info in infos)
            require(declared_size <= uncompressed_limit, "archive: uncompressed size outside policy")
            observed_size = 0
            for info in infos:
                chunks = []
                member_size = 0
                with bundle.open(info, "r") as handle:
                    while True:
                        chunk = handle.read(1024 * 1024)
                        if not chunk:
                            break
                        member_size += len(chunk)
                        observed_size += len(chunk)
                        require(member_size <= info.file_size, f"archive: size mismatch for {info.filename}")
                        require(observed_size <= uncompressed_limit, "archive: expanded beyond policy")
                        chunks.append(chunk)
                require(member_size == info.file_size, f"archive: truncated member {info.filename}")
                members[info.filename] = b"".join(chunks)
    except (OSError, zipfile.BadZipFile, RuntimeError) as error:
        raise ValidationError("archive: invalid ZIP") from error
    return members


def validate_policy(policy, behavior_policy, dependency_lock, evaluator):
    require(policy.get("schema_version") == "graph-evidence/1.0.0", "policy: schema mismatch")
    require(policy.get("mode") == "report-only", "policy: mode is not report-only")
    interpretation = policy.get("interpretation", {})
    require(interpretation.get("composite_quality_score") is False, "policy: composite score enabled")
    require(interpretation.get("merge_gate") is False, "policy: merge gate enabled")
    require(interpretation.get("automatic_refactor") is False, "policy: automatic refactor enabled")
    require(behavior_policy.get("schema_version") == "behavior-policy/1.0.0", "behavior policy: schema mismatch")
    require(
        behavior_policy.get("ci_collection", {}).get("evidence_result") == "inconclusive",
        "behavior policy: CI result must be inconclusive",
    )
    lock_digest = digest_file(dependency_lock)
    require(
        lock_digest == policy.get("runtime", {}).get("dependency_lock_sha256"),
        "policy: dependency lock digest mismatch",
    )
    evaluator_digest = digest_file(evaluator)
    require(
        evaluator_digest == policy.get("ci_adapter", {}).get("evaluator_sha256"),
        "policy: evaluator digest mismatch",
    )
    require(
        policy.get("comparability", {}).get("configuration_change_result") == REBASELINE,
        "policy: rebaseline phrase mismatch",
    )
    return lock_digest, evaluator_digest


def validate_preflight(value, compressed_limit):
    exact_keys(
        value,
        {
            "schema_version", "repository", "repository_id", "workflow_id",
            "run_id", "run_attempt", "event_name", "publish_kind", "pr_number",
            "base_sha", "head_sha", "head_repository_id", "configuration_changed",
            "artifact_id", "artifact_name", "artifact_size", "artifact_digest",
        },
        "preflight",
    )
    require(value["schema_version"] == "architecture-evidence-workflow-run/1.0.0", "preflight: schema mismatch")
    require(isinstance(value["repository"], str) and REPOSITORY_RE.fullmatch(value["repository"]), "preflight: repository invalid")
    for field in ("repository_id", "workflow_id", "run_id", "run_attempt", "head_repository_id", "artifact_id"):
        integer(value[field], f"preflight.{field}", 1)
    integer(value["artifact_size"], "preflight.artifact_size", 1)
    require(value["artifact_size"] <= compressed_limit, "preflight: artifact too large")
    require(value["event_name"] in {"pull_request", "push", "schedule", "workflow_dispatch"}, "preflight: event invalid")
    require(value["publish_kind"] in {"pr", "summary"}, "preflight: publish kind invalid")
    commit_sha(value["head_sha"], "preflight.head_sha")
    require(isinstance(value["configuration_changed"], (bool, type(None))), "preflight: configuration flag invalid")
    require(
        value["artifact_name"] == f"architecture-evidence-v1-run-{value['run_id']}-attempt-{value['run_attempt']}",
        "preflight: artifact name mismatch",
    )
    require(
        value["artifact_digest"] is None
        or (
            isinstance(value["artifact_digest"], str)
            and re.fullmatch(r"sha256:[0-9a-f]{64}", value["artifact_digest"])
        ),
        "preflight: artifact digest invalid",
    )
    if value["publish_kind"] == "pr":
        require(value["event_name"] == "pull_request", "preflight: PR publish event mismatch")
        integer(value["pr_number"], "preflight.pr_number", 1)
        commit_sha(value["base_sha"], "preflight.base_sha")
        require(isinstance(value["configuration_changed"], bool), "preflight: missing PR configuration flag")
    else:
        require(value["event_name"] != "pull_request", "preflight: summary event mismatch")
        require(value["pr_number"] is None and value["base_sha"] is None, "preflight: unexpected PR fields")
        require(value["configuration_changed"] is None, "preflight: unexpected configuration flag")


def validate_digest_map(value, label, expected_keys=None):
    expected = {
        "analyzer", "dependency_lock", "policy", "behavior_policy", "schema",
        "normalizer", "corpus", "harness", "environment", "runtime", "evaluator",
        "configuration",
    } if expected_keys is None else set(expected_keys)
    exact_keys(value, expected, label)
    for key, item in value.items():
        sha256(item, f"{label}.{key}")


def validate_analyzer_identity(value):
    exact_keys(
        value,
        {
            "distribution_name", "version", "distribution_sha256",
            "installed_code_sha256", "measured_file_count", "entrypoint_sha256",
            "interpreter_sha256",
        },
        "run-metadata.analyzer_identity",
    )
    require(
        value["distribution_name"] == "code-review-graph",
        "run-metadata: analyzer distribution name mismatch",
    )
    require(
        isinstance(value["version"], str)
        and 0 < len(value["version"]) <= 100
        and all(ord(character) >= 0x20 for character in value["version"]),
        "run-metadata: analyzer version invalid",
    )
    integer(value["measured_file_count"], "run-metadata.analyzer_identity.measured_file_count", 1)
    for field in (
        "distribution_sha256", "installed_code_sha256", "entrypoint_sha256",
        "interpreter_sha256",
    ):
        sha256(value[field], f"run-metadata.analyzer_identity.{field}")


def validate_dynamic_digests(environment, analyzer, digests):
    exact_keys(
        environment,
        {
            "os", "release", "machine", "python", "python_implementation",
            "runner_image_os", "runner_image_version", "runner_arch",
            "declared_runner", "CRG_SERIAL_PARSE", "CRG_LEIDEN_SEED",
            "PYTHONHASHSEED", "TZ", "git", "analyzer", "crg_invocation",
        },
        "run-metadata.environment",
    )
    for field in (
        "os", "release", "machine", "python", "python_implementation",
        "declared_runner", "CRG_SERIAL_PARSE", "CRG_LEIDEN_SEED",
        "PYTHONHASHSEED", "TZ",
    ):
        require(
            isinstance(environment[field], str) and 0 < len(environment[field]) <= 500,
            f"run-metadata.environment.{field}: invalid string",
        )
    for field in ("runner_image_os", "runner_image_version", "runner_arch"):
        require(
            environment[field] is None
            or (isinstance(environment[field], str) and len(environment[field]) <= 500),
            f"run-metadata.environment.{field}: invalid value",
        )
    exact_keys(
        environment["git"],
        {
            "version", "executable_sha256", "config_nosystem", "config_global",
            "no_replace_objects", "allow_protocol",
        },
        "run-metadata.environment.git",
    )
    require(
        isinstance(environment["git"]["version"], str)
        and 0 < len(environment["git"]["version"]) <= 500,
        "run-metadata.environment.git.version: invalid string",
    )
    sha256(
        environment["git"]["executable_sha256"],
        "run-metadata.environment.git.executable_sha256",
    )
    require(
        environment["git"]["config_nosystem"] == "1"
        and environment["git"]["config_global"] == "/dev/null"
        and environment["git"]["no_replace_objects"] == "1"
        and environment["git"]["allow_protocol"] == "file",
        "run-metadata.environment.git: isolation contract mismatch",
    )
    exact_keys(
        environment["analyzer"],
        {
            "version", "installed_code_sha256", "entrypoint_sha256",
            "interpreter_sha256",
        },
        "run-metadata.environment.analyzer",
    )
    require(
        environment["analyzer"]
        == {
            "version": analyzer["version"],
            "installed_code_sha256": analyzer["installed_code_sha256"],
            "entrypoint_sha256": analyzer["entrypoint_sha256"],
            "interpreter_sha256": analyzer["interpreter_sha256"],
        },
        "run-metadata.environment: analyzer identity mismatch",
    )
    exact_keys(
        environment["crg_invocation"],
        {"build", "status", "visualize", "detect_changes", "network_proxies"},
        "run-metadata.environment.crg_invocation",
    )
    for field in ("build", "status", "visualize", "detect_changes"):
        string_list(
            environment["crg_invocation"][field],
            f"run-metadata.environment.crg_invocation.{field}",
            20,
        )
    require(
        environment["crg_invocation"]["network_proxies"] == "loopback-deny",
        "run-metadata.environment: network isolation mismatch",
    )
    require(
        digests["environment"] == digest_bytes(canonical_bytes(environment)),
        "run-metadata: environment digest mismatch",
    )
    runtime = {
        "actual_environment": environment,
        "entrypoint_sha256": analyzer["entrypoint_sha256"],
        "interpreter_sha256": analyzer["interpreter_sha256"],
    }
    require(
        digests["runtime"] == digest_bytes(canonical_bytes(runtime)),
        "run-metadata: runtime digest mismatch",
    )


def validate_comparability(value, configuration_changed, required_digests, label):
    exact_keys(
        value,
        {"status", "reason", "required_matching_digests", "configuration_changed"},
        label,
    )
    require(value["required_matching_digests"] == required_digests, f"{label}: digest contract mismatch")
    require(value["configuration_changed"] is configuration_changed, f"{label}: configuration flag mismatch")
    if configuration_changed:
        require(value["status"] == "not-comparable" and value["reason"] == REBASELINE, f"{label}: missing rebaseline result")
    else:
        require(value["status"] == "comparable" and value["reason"] is None, f"{label}: unexpected non-comparability")


def validate_parser(value, label):
    exact_keys(
        value,
        {
            "declared_source_files", "parsed_files", "parser_coverage",
            "declared_source_bytes", "parsed_bytes", "parser_coverage_by_bytes",
            "unsupported_files",
        },
        label,
    )
    declared = integer(value["declared_source_files"], f"{label}.declared_source_files")
    parsed = integer(value["parsed_files"], f"{label}.parsed_files")
    require(parsed <= declared, f"{label}: parsed files exceed declared files")
    number(value["parser_coverage"], f"{label}.parser_coverage", 0, 1)
    declared_bytes = integer(value["declared_source_bytes"], f"{label}.declared_source_bytes")
    parsed_bytes = integer(value["parsed_bytes"], f"{label}.parsed_bytes")
    require(parsed_bytes <= declared_bytes, f"{label}: parsed bytes exceed declared bytes")
    number(value["parser_coverage_by_bytes"], f"{label}.parser_coverage_by_bytes", 0, 1)
    string_list(value["unsupported_files"], f"{label}.unsupported_files")


def validate_artifacts(raw_artifacts, parsed_artifacts, metadata, policy, behavior_policy, preflight, trusted_digests):
    schemas = {
        "snapshot": policy["ci_adapter"]["snapshot_schema"],
        "comparison": policy["ci_adapter"]["comparison_schema"],
        "behavior": policy["ci_adapter"]["behavior_schema"],
        "delphi": policy["ci_adapter"]["delphi_schema"],
        "artifact": policy["publication"]["artifact_schema"],
    }
    exact_keys(
        metadata,
        {
            "schema_version", "artifact_type", "schema_compatibility", "repository",
            "repository_id", "head_repository_id", "event_name", "run_id",
            "run_attempt", "pr_number", "base_sha", "head_sha",
            "configuration_changed", "comparability", "digests", "environment",
            "analyzer_identity", "history", "behavior", "delphi", "files",
            "artifact_digest",
        },
        "run-metadata",
    )
    validate_self_digest(metadata, "run-metadata")
    require(metadata["artifact_type"] == "architecture-evidence-bundle", "run-metadata: artifact type mismatch")
    identity_fields = {
        "repository": preflight["repository"],
        "repository_id": preflight["repository_id"],
        "head_repository_id": preflight["head_repository_id"],
        "event_name": preflight["event_name"],
        "run_id": preflight["run_id"],
        "run_attempt": preflight["run_attempt"],
        "pr_number": preflight["pr_number"],
        "head_sha": preflight["head_sha"],
    }
    for field, expected in identity_fields.items():
        require(metadata[field] == expected, f"run-metadata: {field} mismatch")
    commit_sha(metadata["base_sha"], "run-metadata.base_sha")
    if preflight["publish_kind"] == "pr":
        require(
            metadata["base_sha"] == preflight["base_sha"],
            "run-metadata: pull-request base mismatch",
        )
    require(
        isinstance(metadata["configuration_changed"], bool),
        "run-metadata: configuration flag is not Boolean",
    )
    artifact_configuration_changed = metadata["configuration_changed"]
    if preflight["configuration_changed"] is None:
        require(
            preflight["publish_kind"] == "summary",
            "run-metadata: only trusted summaries may derive the configuration flag",
        )
        configuration_changed = artifact_configuration_changed
    else:
        # Either independently derived signal is sufficient to suppress metrics.
        # The artifact flag catches base-only guarded changes; the preflight flag
        # deliberately fails closed for GitHub's changed-file enumeration cap.
        configuration_changed = (
            preflight["configuration_changed"] or artifact_configuration_changed
        )
    if artifact_configuration_changed:
        require(
            isinstance(metadata["schema_version"], str)
            and 0 < len(metadata["schema_version"]) <= 200,
            "run-metadata: invalid changed schema declaration",
        )
        require(
            isinstance(metadata["schema_compatibility"], str)
            and 0 < len(metadata["schema_compatibility"]) <= 500,
            "run-metadata: invalid changed compatibility declaration",
        )
        comparison_contract = metadata["comparability"]
        require(isinstance(comparison_contract, dict), "run-metadata: comparability missing")
        required_digests = string_list(
            comparison_contract.get("required_matching_digests"),
            "run-metadata.comparability.required_matching_digests",
            32,
        )
        require(
            len(required_digests) == len(set(required_digests))
            and all(re.fullmatch(r"[a-z][a-z0-9_]{0,63}", item) for item in required_digests),
            "run-metadata: invalid changed digest contract",
        )
        require(
            isinstance(metadata["digests"], dict)
            and set(metadata["digests"]) == set(required_digests),
            "run-metadata: changed digest keys do not match the declared contract",
        )
        validate_digest_map(
            metadata["digests"], "run-metadata.digests", required_digests
        )
    else:
        require(metadata["schema_version"] == schemas["artifact"], "run-metadata: schema mismatch")
        require(metadata["schema_compatibility"] == policy["ci_adapter"]["schema_compatibility"], "run-metadata: compatibility declaration mismatch")
        required_digests = policy["comparability"]["required_matching_digests"]
        validate_digest_map(metadata["digests"], "run-metadata.digests")
    validate_comparability(metadata["comparability"], artifact_configuration_changed, required_digests, "run-metadata.comparability")

    exact_keys(metadata["files"], EXPECTED_MEMBERS - {"run-metadata.json"}, "run-metadata.files")
    for name, expected in metadata["files"].items():
        sha256(expected, f"run-metadata.files.{name}")
        require(digest_bytes(raw_artifacts[name]) == expected, f"run-metadata: file digest mismatch for {name}")
    require(raw_artifacts["history.ndjson"] == b"", "history.ndjson: CI placeholder must be empty")

    analyzer = metadata["analyzer_identity"]
    validate_analyzer_identity(analyzer)
    snapshot = parsed_artifacts["snapshot.json"]
    comparison = parsed_artifacts["comparison.json"]
    behavior = parsed_artifacts["behavior-evidence.json"]
    delphi = parsed_artifacts["delphi-rounds.json"]
    if artifact_configuration_changed:
        portable_schemas = {
            "architecture-snapshot/1.0.0", "architecture-comparison/1.0.0",
            "behavior-evidence/1.0.0", "delphi-rounds/1.0.0",
        }
        for label, value in (
            ("snapshot", snapshot), ("comparison", comparison),
            ("behavior-evidence", behavior), ("delphi-rounds", delphi),
        ):
            validate_self_digest(value, label)
            require(
                isinstance(value.get("schema_version"), str)
                and value["schema_version"] not in portable_schemas,
                f"{label}: invalid or portable-masquerading CI schema",
            )
        return True, snapshot, comparison

    exact_keys(metadata["history"], {"status", "case_count"}, "run-metadata.history")
    require(metadata["history"] == {"status": "unavailable", "case_count": 0}, "run-metadata: history status mismatch")
    require(metadata["behavior"] == {"evidence_result": "inconclusive"}, "run-metadata: behavior status mismatch")
    require(metadata["delphi"] == {"status": "unavailable"}, "run-metadata: Delphi status mismatch")

    common = metadata["digests"]
    if not artifact_configuration_changed:
        for key, expected in trusted_digests.items():
            require(common[key] == expected, f"run-metadata: trusted {key} digest mismatch")
    validate_dynamic_digests(metadata["environment"], analyzer, common)
    require(analyzer["version"] == policy["analyzer"]["version"], "run-metadata: analyzer version mismatch")
    require(analyzer["distribution_sha256"] == policy["analyzer"]["distribution_sha256"], "run-metadata: analyzer distribution mismatch")
    require(analyzer["installed_code_sha256"] == policy["analyzer"]["installed_code_sha256"], "run-metadata: analyzer code mismatch")

    typed_artifacts = (
        ("snapshot", snapshot, "snapshot"),
        ("comparison", comparison, "comparison"),
        ("behavior-evidence", behavior, "behavior-evidence"),
        ("delphi-rounds", delphi, "delphi-rounds"),
    )
    for label, value, artifact_type in typed_artifacts:
        require(isinstance(value.get("schema_version"), str), f"{label}: schema missing")
        require(value.get("artifact_type") == artifact_type, f"{label}: artifact type mismatch")
        validate_self_digest(value, label)
        validate_digest_map(value["digests"], f"{label}.digests", common.keys())
        require(value["digests"] == common, f"{label}: evaluator digests differ from run metadata")
    require(snapshot.get("revision") == preflight["head_sha"], "snapshot: head mismatch")
    require(comparison.get("base") == metadata["base_sha"] and comparison.get("head") == preflight["head_sha"], "comparison: revisions mismatch")
    require(behavior.get("base") == metadata["base_sha"] and behavior.get("head") == preflight["head_sha"], "behavior-evidence: revisions mismatch")
    validate_comparability(comparison["comparability"], artifact_configuration_changed, required_digests, "comparison.comparability")
    exact_keys(snapshot, {"schema_version", "artifact_type", "adapter_scope", "revision", "digests", "parser", "topology", "crg", "behavior", "delphi", "limitations", "artifact_digest"}, "snapshot")
    exact_keys(comparison, {"schema_version", "artifact_type", "adapter_scope", "base", "head", "digests", "comparability", "parser", "topology", "crg", "behavior", "delphi", "limitations", "artifact_digest"}, "comparison")
    exact_keys(behavior, {"schema_version", "artifact_type", "base", "head", "digests", "behavior", "artifact_digest"}, "behavior-evidence")
    exact_keys(delphi, {"schema_version", "artifact_type", "digests", "delphi", "artifact_digest"}, "delphi-rounds")
    require(snapshot["schema_version"] == schemas["snapshot"], "snapshot: schema mismatch")
    require(comparison["schema_version"] == schemas["comparison"], "comparison: schema mismatch")
    require(behavior["schema_version"] == schemas["behavior"], "behavior-evidence: schema mismatch")
    require(delphi["schema_version"] == schemas["delphi"], "delphi-rounds: schema mismatch")
    validate_parser(snapshot["parser"], "snapshot.parser")
    exact_keys(comparison["parser"], {"base", "head", "coverage_delta", "comparison_status", "comparison_reason"}, "comparison.parser")
    validate_parser(comparison["parser"]["base"], "comparison.parser.base")
    validate_parser(comparison["parser"]["head"], "comparison.parser.head")
    if artifact_configuration_changed:
        require(comparison["parser"]["coverage_delta"] is None, "comparison.parser: delta present while non-comparable")
        require(comparison["parser"]["comparison_status"] == "not-comparable" and comparison["parser"]["comparison_reason"] == REBASELINE, "comparison.parser: missing rebaseline result")
    else:
        number(comparison["parser"]["coverage_delta"], "comparison.parser.coverage_delta", -1, 1)
        require(comparison["parser"]["comparison_status"] == "comparable" and comparison["parser"]["comparison_reason"] is None, "comparison.parser: invalid comparable status")
    require(snapshot["behavior"].get("evidence_result") == "inconclusive", "snapshot: behavior must be inconclusive")
    require(comparison["behavior"].get("evidence_result") == "inconclusive", "comparison: behavior must be inconclusive")
    require(behavior["behavior"].get("evidence_result") == "inconclusive", "behavior-evidence: result mismatch")
    require(behavior["behavior"].get("executed_harness_digest") is None, "behavior-evidence: CI may not claim harness execution")
    require(delphi["delphi"].get("status") == "unavailable" and delphi["delphi"].get("rounds") == [], "delphi-rounds: CI panel claim is invalid")
    return configuration_changed, snapshot, comparison


def trusted_digest_contract(
    policy,
    behavior_policy,
    lock_digest,
    evaluator_digest,
    analysis_workflow_digest,
    publisher_workflow_digest,
):
    schemas = {
        "policy": policy["schema_version"],
        "snapshot": policy["ci_adapter"]["snapshot_schema"],
        "comparison": policy["ci_adapter"]["comparison_schema"],
        "behavior": policy["ci_adapter"]["behavior_schema"],
        "delphi": policy["ci_adapter"]["delphi_schema"],
        "artifact": policy["publication"]["artifact_schema"],
    }
    corpus = {
        "include": policy["corpus"]["include"],
        "exclude": policy["corpus"]["exclude"],
        "source_suffixes": policy["corpus"]["source_suffixes"],
        "template_suffixes": policy["corpus"]["template_suffixes"],
    }
    configuration = {
        "schema_version": policy["ci_adapter"]["configuration_schema"],
        "files": {
            ".github/workflows/graph-metrics.yml": analysis_workflow_digest,
            ".github/workflows/graph-metrics-publish.yml": publisher_workflow_digest,
        },
    }
    return {
        "analyzer": policy["analyzer"]["installed_code_sha256"],
        "dependency_lock": lock_digest,
        "policy": digest_bytes(canonical_bytes(policy)),
        "behavior_policy": digest_bytes(canonical_bytes(behavior_policy)),
        "schema": digest_bytes(canonical_bytes(schemas)),
        "normalizer": digest_bytes(canonical_bytes(policy["ci_adapter"]["normalizer"])),
        "corpus": digest_bytes(canonical_bytes(corpus)),
        "harness": behavior_policy["harness"]["digest"],
        "evaluator": evaluator_digest,
        "configuration": digest_bytes(canonical_bytes(configuration)),
    }


def safe_metric(value, label):
    return number(value, label, 0)


def render_markdown(preflight, configuration_changed, snapshot, comparison, max_bytes):
    if configuration_changed:
        lines = [
            MARKER,
            "# Architecture evidence (report only)",
            "",
            f"- Head: `{preflight['head_sha']}`",
            f"- Comparability: **{REBASELINE}**",
            "- Evaluator or policy infrastructure changed in this run.",
            "- No machine delta, behavior claim, or panel judgment is published from this run.",
            "",
            "This report is not a merge gate and contains no composite quality score.",
            "",
        ]
    else:
        parser = snapshot["parser"]
        projections = snapshot.get("topology", {}).get("projections", {})
        imports = projections.get("imports", {})
        require(isinstance(imports, dict), "snapshot: imports projection missing")
        machine = {
            "largest SCC": safe_metric(imports.get("largest_scc_nodes"), "imports.largest_scc_nodes"),
            "cyclic nodes": safe_metric(imports.get("cyclic_nodes"), "imports.cyclic_nodes"),
            "cycle mass": safe_metric(imports.get("cycle_mass"), "imports.cycle_mass"),
            "excess cyclic edges": safe_metric(imports.get("excess_cyclic_edges"), "imports.excess_cyclic_edges"),
            "cross-boundary cyclic edges": safe_metric(imports.get("cross_boundary_cyclic_edges"), "imports.cross_boundary_cyclic_edges"),
        }
        comparison_imports = comparison.get("topology", {}).get("comparison", {}).get("imports", {})
        require(isinstance(comparison_imports, dict), "comparison: imports projection missing")
        new_edges = comparison_imports.get("new_cycle_forming_edges")
        require(isinstance(new_edges, list), "comparison: new cycle edge list missing")
        risk = snapshot.get("crg", {}).get("risk", {})
        risk_value = risk.get("risk_score") if risk.get("status") == "measured" else "unavailable"
        if risk_value != "unavailable":
            number(risk_value, "snapshot.crg.risk.risk_score")
        lines = [
            MARKER,
            "# Architecture evidence (report only)",
            "",
            f"- Base: `{comparison['base']}`",
            f"- Head: `{preflight['head_sha']}`",
            "- Comparability: **comparable**",
            f"- Parser coverage: {parser['parsed_files']}/{parser['declared_source_files']} ({parser['parser_coverage'] * 100:.3f}%)",
            f"- Unsupported files: {len(parser['unsupported_files'])}",
            "",
            "## Machine signal",
            "",
            *[f"- Imports {name}: {value}" for name, value in machine.items()],
            f"- New cycle-forming edges: {len(new_edges)}",
            f"- CRG heuristic risk: {risk_value} (separate heuristic lane)",
            "",
            "## Behavioral evidence",
            "",
            "- Result: **inconclusive**; the CI parser did not execute the frozen black-box harness.",
            "",
            "## Delphi-inspired judgment",
            "",
            "- Status: **unavailable in CI**; no model credentials or synthetic panel responses were used.",
            "",
            "This report has no composite quality score, merge gate, automatic refactor, or behavioral-equivalence claim.",
            "",
        ]
    text = "\n".join(lines)
    remainder = text[len(MARKER):]
    require("<" not in remainder and ">" not in remainder and "\x00" not in text, "rendered Markdown contains unsafe characters")
    require(all(character in "\n\r" or ord(character) >= 0x20 for character in text), "rendered Markdown contains controls")
    encoded = text.encode("utf-8")
    require(len(encoded) <= max_bytes, "rendered Markdown exceeds policy")
    return encoded


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--archive", required=True)
    parser.add_argument("--preflight", required=True)
    parser.add_argument("--policy", required=True)
    parser.add_argument("--behavior-policy", required=True)
    parser.add_argument("--dependency-lock", required=True)
    parser.add_argument("--evaluator", required=True)
    parser.add_argument("--analysis-workflow", required=True)
    parser.add_argument("--publisher-workflow", required=True)
    parser.add_argument("--output-dir", required=True)
    args = parser.parse_args()

    policy = load_json_file(args.policy, "policy")
    behavior_policy = load_json_file(args.behavior_policy, "behavior policy")
    preflight = load_json_file(args.preflight, "preflight")
    lock_digest, evaluator_digest = validate_policy(
        policy, behavior_policy, args.dependency_lock, args.evaluator
    )
    publication = policy["publication"]
    compressed_limit = integer(publication["artifact_max_compressed_bytes"], "policy.compressed_limit", 1)
    uncompressed_limit = integer(publication["artifact_max_uncompressed_bytes"], "policy.uncompressed_limit", 1)
    markdown_limit = integer(publication["markdown_max_bytes"], "policy.markdown_limit", 1)
    validate_preflight(preflight, compressed_limit)
    archive_path = Path(args.archive)
    require(
        archive_path.is_file()
        and not archive_path.is_symlink()
        and archive_path.stat().st_size == preflight["artifact_size"],
        "archive: downloaded size does not match the workflow artifact identity",
    )
    if preflight["artifact_digest"] is not None:
        require(
            digest_file(args.archive) == preflight["artifact_digest"].split(":", 1)[1],
            "archive: workflow artifact digest mismatch",
        )
    raw_members = read_archive(args.archive, compressed_limit, uncompressed_limit)
    parsed = {name: loads_json_strict(raw_members[name], name) for name in JSON_MEMBERS}
    trusted = trusted_digest_contract(
        policy,
        behavior_policy,
        lock_digest,
        evaluator_digest,
        digest_regular_file(args.analysis_workflow, "analysis workflow"),
        digest_regular_file(args.publisher_workflow, "publisher workflow"),
    )
    configuration_changed, snapshot, comparison = validate_artifacts(
        raw_members,
        parsed,
        parsed["run-metadata.json"],
        policy,
        behavior_policy,
        preflight,
        trusted,
    )
    rendered = render_markdown(
        preflight, configuration_changed, snapshot, comparison, markdown_limit
    )
    output = Path(args.output_dir)
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    destination = output / "comment.md"
    destination.write_bytes(rendered)
    os.chmod(destination, 0o600)


if __name__ == "__main__":
    try:
        main()
    except ValidationError as error:
        print(f"publisher validation failed: {error}", file=sys.stderr)
        raise SystemExit(1)
