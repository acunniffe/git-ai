# Attribution Cookbook: Surviving Squash Merges

## Problem

Sealed attribution notes identify AI-written line ranges by commit SHA.
A hosting platform's squash merge creates a new commit that git-ai never
observed, severing the pre-land commits and their notes from the landed
commit's ancestry.

## Opt-in content fingerprints

Enable fingerprints with:

```bash
git-ai config set attribution_fingerprints true
```

Each hook and sink event then includes an `attributions` array:

```json
{
  "schema_version": 2,
  "commit_sha": "abc123",
  "attributions": [
    {
      "file": "src/calc.py",
      "session_id": "session-uuid",
      "model": "claude-sonnet-4",
      "tool": "cursor",
      "line_ranges": [[10, 16], [40, 42]],
      "fingerprints": ["a1b2c3d4e5f6", "..."],
      "fingerprints_complete": true
    }
  ]
}
```

### Permanent fingerprint contract

```text
fingerprint(line) =
  hex(sha256(line with trailing "\n" and "\r" stripped))[:12]
```

Only newline characters are stripped. Spaces and tabs remain significant.
Changing this normalization requires a `schema_version` bump.

Fingerprints are computed from git-ai's checkpoint blob—the content the
agent wrote—not from the committed file. If a human edits an AI-written
line before commit, its checkpoint fingerprint will not match the landed
line, so the line correctly drops out of AI attribution.

For each attribution block, fingerprints follow ascending line order
across the union of `line_ranges`:

```text
len(fingerprints) ==
  sum(end - start + 1 for [start, end] in line_ranges)
```

If content is unavailable, `fingerprints_complete` is `false`; consumers
must not interpret missing fingerprints as human authorship.

## Consumer alignment

For each `(commit, file, session)` block:

```text
S = emitted fingerprints in order
L = fingerprints of the landed file's lines in order
cursor = 0

for fingerprint in S:
  find the first unclaimed index j >= cursor where L[j] == fingerprint
  if found:
    attribute landed line j to this block
    claim j
    cursor = j + 1
  otherwise:
    treat the source line as edited or removed
```

Process blocks in commit-time order and share the claimed-line set across
blocks. Do not use set membership: repeated lines are common, and ignoring
order can attribute the wrong occurrence.
