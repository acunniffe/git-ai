#!/usr/bin/env python3
"""Generate the exhaustive Graphite-workflow scenario matrix.

Single source of truth for the scenario suite: emits
  - scenarios.json  (consumed by the Rust harness via include_str!)
  - the scenario table appended to PLAN.md (run with --markdown)

Scenario IDs are stable: GT-<FAMILY>-<NNN>. Do not renumber; append only.
"""
import itertools
import json
import sys

# Dimension vocabularies -----------------------------------------------------

STACKS = {
    "single": "one branch off trunk, 1 AI commit",
    "stack2_bot": "2-branch stack, change under test on the BOTTOM branch",
    "stack2_top": "2-branch stack, change under test on the TOP branch",
    "stack3_mid": "3-branch stack, change under test on the MIDDLE branch",
    "stack3_all": "3-branch stack, AI commits on ALL branches",
}

ATTRIBUTION = {
    "committed": "AI work committed with notes before the workflow",
    "uncommitted": "AI checkpoint in the working log, not yet committed",
    "mixed": "committed AI commit plus uncommitted AI edits on top",
    "multifile": "one commit touching 3 AI files (rename-adjacent shapes)",
}

TRUNK = {
    "ff": "trunk unchanged or clean fast-forward",
    "diverged": "trunk advanced with non-overlapping changes (real restack)",
    "overlap": "trunk advanced touching the same file regions (conflict fuel)",
}

OBSERVATION = {
    "traced": "daemon observes every command (normal laptop)",
    "blind": "rewrite phase executed raw (trace2 off), daemon restarted after "
             "-- reconciliation/replay territory",
}

REPEAT = {
    "once": "run the workflow once",
    "twice": "run the workflow twice back-to-back (idempotency; second run "
             "is usually a no-op and must not disturb notes)",
}

# Workflow families and their applicable dimensions ---------------------------
# (workflow, stacks, attribution, trunk, observation, repeat)

FAMILIES = [
    # fam, description, stacks, attrs, trunks, observations, repeats
    ("SYNC_FF",
     "gt sync trunk fast-forward: fetch; update-ref trunk (or `reset -q --keep` "
     "when trunk/branch is checked out); stash create/ls-files snapshot around it",
     ["single", "stack2_bot"], list(ATTRIBUTION), ["ff"], list(OBSERVATION),
     list(REPEAT)),
    ("SYNC_RESTACK",
     "gt sync with restack: fetch + trunk move, then per-branch synthetic-base "
     "commit-tree -> merge-tree --allow-unrelated-histories -> commit-tree(real "
     "msg) -> update-ref, bottom-up through the stack",
     list(STACKS), list(ATTRIBUTION), ["diverged", "overlap"],
     list(OBSERVATION), list(REPEAT)),
    ("RESTACK",
     "gt restack without fetch: same rewrite core as SYNC_RESTACK, no remote",
     list(STACKS), list(ATTRIBUTION), ["diverged"], list(OBSERVATION),
     ["once"]),
    ("CREATE",
     "gt create: stash-wrapped branch creation + commit (stash create; "
     "checkout -b; commit; metadata refs)",
     ["single", "stack2_top"], list(ATTRIBUTION), ["ff"], list(OBSERVATION),
     ["once"]),
    ("MODIFY",
     "gt modify: amend HEAD (commit --amend shape) then restack descendants "
     "via the rewrite core",
     ["single", "stack2_bot", "stack3_mid"], list(ATTRIBUTION), ["ff"],
     list(OBSERVATION), list(REPEAT)),
    ("SUBMIT",
     "gt submit: restack if needed, then push --force-with-lease... "
     "--no-verify --atomic per branch",
     ["single", "stack2_bot", "stack3_all"], ["committed", "mixed"],
     ["ff", "diverged"], ["traced"], ["once"]),
    ("CONFLICT_CONTINUE",
     "restack hits a real conflict (merge-tree unresolvable / "
     "cleanRebaseMergeTree failure) -> gt falls back to real git rebase -> "
     "user resolves -> gt continue",
     ["single", "stack2_bot", "stack3_mid"], list(ATTRIBUTION), ["overlap"],
     list(OBSERVATION), ["once"]),
    ("CONFLICT_ABORT",
     "same conflict entry, then gt abort -> git rebase --abort + gt state "
     "restore; branch must return to pre-workflow tip with notes intact",
     ["single", "stack2_bot"], list(ATTRIBUTION), ["overlap"],
     list(OBSERVATION), ["once"]),
    ("UNDO",
     "gt undo of a completed restack: reset -q --keep <pre-restack-tip> "
     "moving the branch backward (fork-point-spanning ranges)",
     ["single", "stack2_bot", "stack3_mid"], ["committed", "mixed"],
     ["diverged"], list(OBSERVATION), list(REPEAT)),
    ("MOVE_ONTO",
     "gt move --onto: rewrite core applied with a different parent (not "
     "trunk); descendants restacked",
     ["stack2_bot", "stack3_mid"], ["committed", "uncommitted"], ["ff"],
     list(OBSERVATION), ["once"]),
    ("HOUSEKEEPING",
     "gt checkout/track/trunk/info: metadata-only flows -- control group, "
     "must never touch attribution",
     ["single", "stack3_all"], ["committed", "uncommitted"], ["ff"],
     ["traced"], ["twice"]),
    ("FLAVOR_17X",
     "gt 1.7.x shapes: hash-object -w --stdin storms + eager metadata "
     "update-refs interleaved into the SYNC_RESTACK core",
     ["single", "stack2_bot"], ["committed", "uncommitted"], ["diverged"],
     ["traced"], ["once"]),
    # --- appended families (IDs stable; append-only) ---
    ("WORKTREE",
     "SYNC_RESTACK and MODIFY cores executed from a LINKED WORKTREE "
     "(.claude/worktrees pattern observed in production logs): per-worktree "
     "HEAD reflog, per-worktree working log, shared refs",
     list(STACKS), list(ATTRIBUTION), ["diverged"], list(OBSERVATION),
     ["once"]),
    ("REFSTDIN",
     "SYNC_RESTACK core but all branch moves applied via a single "
     "`update-ref --stdin` batch (binary-evidenced flavor): the daemon sees "
     "ONE command move many refs",
     list(STACKS), list(ATTRIBUTION), ["diverged"], ["traced", "blind"],
     ["once"]),
    ("RENAME",
     "AI commit renames a file (and edits it) then goes through the restack "
     "core; rename-following in note translation under merge-tree rewrites",
     ["single", "stack2_bot", "stack3_mid"], ["committed", "multifile"],
     ["diverged"], list(OBSERVATION), ["once"]),
    ("MERGE_IN_STACK",
     "a merge commit inside the stack (pull-merge shape) goes through the "
     "restack core; exercises derive_merge_commit_mappings",
     ["stack2_bot", "stack3_mid"], ["committed"], ["diverged"],
     list(OBSERVATION), ["once"]),
    ("PARTIAL_STAGE",
     "workflow starts with a working tree mixing staged AI edits, unstaged "
     "AI edits, and untracked files (`stash create` + ls-files snapshot "
     "boundary conditions)",
     ["single", "stack2_bot"], ["uncommitted", "mixed"], ["ff", "diverged"],
     list(OBSERVATION), ["once"]),
    ("LIFECYCLE",
     "full-chain composites: create -> modify -> sync(restack) -> submit; "
     "create -> sync(conflict->continue) -> modify -> submit; "
     "create -> sync -> undo -> sync again; attribution must survive the "
     "entire chain end-to-end",
     ["stack2_bot", "stack3_all"], list(ATTRIBUTION), ["diverged"],
     list(OBSERVATION), list(REPEAT)),
    ("NESTED_CONFLICT",
     "conflicts at TWO stack levels in one sync: resolve+continue the first, "
     "then hit and resolve/abort the second",
     ["stack2_bot", "stack3_mid"], ["committed", "mixed"], ["overlap"],
     list(OBSERVATION), ["once"]),
]


def enumerate_scenarios():
    scenarios = []
    for fam, desc, stacks, attrs, trunks, observations, repeats in FAMILIES:
        n = 0
        for stack, attr, trunk, obs, rep in itertools.product(
                stacks, attrs, trunks, observations, repeats):
            n += 1
            scenarios.append({
                "id": f"GT-{fam}-{n:03d}",
                "family": fam,
                "stack": stack,
                "attribution": attr,
                "trunk": trunk,
                "observation": obs,
                "repeat": rep,
            })
    return scenarios


def main():
    scenarios = enumerate_scenarios()
    if "--markdown" in sys.argv:
        fams = {}
        for s in scenarios:
            fams.setdefault(s["family"], []).append(s)
        print(f"Total scenarios: {len(scenarios)}\n")
        for fam, items in fams.items():
            print(f"### {fam} ({len(items)} scenarios)\n")
            print("| id | stack | attribution | trunk | observation | repeat |")
            print("|---|---|---|---|---|---|")
            for s in items:
                print(f"| {s['id']} | {s['stack']} | {s['attribution']} "
                      f"| {s['trunk']} | {s['observation']} | {s['repeat']} |")
            print()
    else:
        json.dump({
            "dimensions": {
                "stack": STACKS, "attribution": ATTRIBUTION, "trunk": TRUNK,
                "observation": OBSERVATION, "repeat": REPEAT,
            },
            "families": {f[0]: f[1] for f in FAMILIES},
            "scenarios": scenarios,
        }, sys.stdout, indent=1)
        sys.stdout.write("\n")


if __name__ == "__main__":
    main()
