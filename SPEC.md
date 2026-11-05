# loch format, version 1

**Status: draft.** The derived-file format and the Claude Code converter
(§3–§8) are implemented in `loch/format.py` and `loch/ingest.py`. The storage,
pin and native-turn sections (§9–§11) describe intended shapes that nothing
writes yet. Questions this draft doesn't settle are collected in §13 rather
than guessed at.

## 1. What this is for

A conversation between people and agents should work like source code: owned
locally, content-addressed, forkable at any point, shareable without a central
server. loch is that layer — the `git` to an editor's `vim`. It knows nothing
about any particular UI.

## 2. Three layers

```
raw source   (agent-native, immutable, content-addressed)   ← what must survive
    │  converter (id + version)
    ▼
derived file (loch JSONL: turns, content, facts, heads)     ← regenerable
    │
    ▼
tools        (Ness, review UIs, sync, merge)                ← read loch only
```

- **Raw source is the source of truth.** It is kept byte for byte. Claude
  Code's transcript stays Claude Code's format; loch never edits it.
- **A derived file is one converter's understanding of one source at one
  time.** It records which converter produced it. When a converter improves,
  derived files are regenerated from the raw source; they are never migrated.
- **Tools read only the derived format.** Each new agent format costs one
  converter into loch, not one per tool.

## 3. Encoding and ids

- **Canonical JSON:** UTF-8, keys sorted, separators `,` and `:` with no
  whitespace, non-ASCII written literally, NaN and Infinity rejected. Lone
  surrogates are encoded with `surrogatepass` rather than refused, since real
  transcripts contain them.
- **Ids:** `sha256:` followed by 64 lowercase hex digits, the SHA-256 of the
  canonical JSON. The prefix names the algorithm so a future one can coexist.
- **Numbers:** floats use the shortest round-trip form, which matches RFC 8785
  for every value without an exponent (§13.4).

## 4. The turn

A turn is one node in the conversation graph. Its id is the hash of its
**core**:

```json
{
  "loch": 1,
  "kind": "message",
  "author": {"kind": "human", "id": "alice"},
  "parents": ["sha256:…"],
  "content": "sha256:…"
}
```

| field | meaning |
| --- | --- |
| `loch` | Format major version. A change to the core's meaning bumps it. |
| `kind` | `message`, `tool-result`, `context`, `event`, `compaction`, and reserved for native turns, `approval` and `note`. |
| `author` | `{"kind": "human" \| "agent" \| "tool" \| "system", ...}`. A human may carry `id`; agent, tool and system authors carry `agent` (e.g. `claude-code`). Absent fields are omitted, never `null`. |
| `parents` | Ordered list of parent turn ids. Empty means a root; two or more means a merge. |
| `content` | The id of the content object, never the content itself. |

The core holds the content's **hash**, not the content. So content can be
deleted (tombstoned) while every id that depends on it, including every
descendant, stays verifiable. Turn ids form a Merkle DAG: a turn's id
transitively covers its whole ancestry.

Deliberately **not** in the core: timestamps, the source record's uuid, model
name, branch, cwd. Two records that say the same thing, by the same author,
from the same parent are the same turn.

### Content

```json
{"blocks": [ ... ]}
```

Its id is the hash of exactly that object. Block types in v1:

| type | fields |
| --- | --- |
| `text` | `text` |
| `thinking` | `text` |
| `redacted-thinking` | — |
| `tool-call` | `call` (the agent's call id), `name`, `input` |
| `tool-result` | `call`, `content` (blocks), `error: true` when the tool failed |
| `image` | `source` (agent-native image source, inline) |
| `x-<agent>:<name>` | Agent-specific payload, passed through unconverted |

Unknown agent blocks are **passed through** as `x-<agent>:block` rather than
dropped. A later converter can learn them, and someone holding only the
derived file hasn't lost anything.

### Facts

Per-turn metadata that is not identity: `line` and `uuid` of the source
record, `alsoUuids` (other records that collapsed into this turn),
`timestamp`, `messageId`, `model`, `gitBranch`, `version`, `cwd`
(normalized), `record` (the source record type), `subtype`, `danglingParent`.
Facts may differ between two derivations of the same turn.

## 5. Normalization

Turn ids must not depend on which machine, directory or session id a
transcript happened to have. Before hashing, every string in content and in
facts, including object keys, is put through one left-to-right pass:

| replaced | with | guard |
| --- | --- | --- |
| a literal `{{loch:` | `{{loch:esc}}` | none |
| session cwd | `{{loch:root}}` | path-component boundary |
| home directory | `{{loch:home}}` | path-component boundary |
| Claude's encoded cwd slug | `{{loch:root-slug}}` | whole encoded component |
| session id | `{{loch:session}}` | none |

The pass is single and longest-match-first, with the same boundary guards
`restore` uses (see `rewrite.py` and CLAUDE.md). So a session moved with
`loch restore` normalizes back to identical bytes. The escape makes the mapping
reversible: substituting the origin back in, `{{loch:esc}}` first, restores
the original exactly. When cwd equals home, only the root pair applies.

The origin values themselves appear only in the derived file's header.

## 6. Heads

```json
{"type": "heads", "current": "sha256:…", "leaves": ["sha256:…", "…"]}
```

- `leaves` are turns with no children, in file order. A rewind inside one
  session leaves more than one.
- `current` is the head the source agent considered live, if it says so.
  Claude Code's `last-prompt.leafUuid` does; otherwise it is the last record.

Named branches are local refs over turn ids, like git branch names, and are
outside the derived file (§13.15).

## 7. Derived file

JSONL in canonical encoding, one object per line, in this order:

1. **One header:**

   ```json
   {"type": "header", "loch": 1,
    "converter": {"id": "claude-code", "version": 1},
    "source": {"id": "sha256:…", "agent": "claude-code",
               "files": [{"path": "transcript.jsonl", "sha256": "sha256:…", "bytes": 123}]},
    "origin": {"cwd": "…", "home": "…", "sessionId": "…"},
    "author": {"kind": "human", "id": "alice"},
    "session": {"title": "…", "aiTitle": "…"}}
   ```

   - `source.id` is the hash of `{agent, files}`, with files in path order.
   - `origin` is the only machine-specific part of the file.

2. **Turn lines**, parents always before children:

   ```json
   {"type": "turn", "id": "sha256:…", "turn": {core},
    "content": {"blocks": [...]}, "facts": {...}}
   ```

   A turn line without `content` is a **tombstone**: the id and core are
   intact, the content is gone.

3. **One heads line.**

A reader must recompute every turn id and every present content id, and must
reject a turn that comes before one of its parents. A file that reads back is
internally consistent, not merely well-formed.

The file is named `<source-hex>.<converter-id>-v<converter-version>.jsonl`.
Ingesting the same source with the same converter and author is
byte-identical.

## 8. Claude Code converter, v1

**Source.** A session is `~/.claude/projects/<slug>/<session-id>.jsonl` plus
everything under `<session-id>/`: subagent transcripts in `subagents/` and
spilled tool output in `tool-results/`. The manifest hashes all of it as
`transcript.jsonl` and `sidecar/<relative path>`. In v1 only the transcript is
converted to turns (§13.9).

**One chain record is one turn.** A chain record is any record with a
`uuid`. Its parent is the turn of its `parentUuid`. Consequences measured
against a real corpus of 3,810 sessions:

- **A single API message is often several turns.** Claude Code writes an
  assistant message one content block per record, and does so for 56% of
  messages, with tool results between blocks in 12% of those. `facts.messageId`
  regroups them.
- **A file is already a tree.** Rewinds and retries leave branches (126 of
  3,000 files), and they survive as multiple leaves.
- **Compaction is linked across.** A `compact_boundary` has
  `parentUuid: null` and names its predecessor in `logicalParentUuid`, which
  v1 follows, so a compacted session stays one connected graph.
- **Parents are ordered before children** using file order as the tiebreak.
  A parent that never appears makes the record a root with
  `facts.danglingParent`.

| Claude record | kind | author |
| --- | --- | --- |
| `assistant` | `message` | agent |
| `user` with a `tool_result` block | `tool-result` | tool |
| `user` with `isMeta` or `isCompactSummary` | `context` | system |
| other `user` | `message` | the ingesting human |
| `attachment` | `context` | system |
| `system`, `compact_boundary` subtype | `compaction` | system |
| other `system` | `event` | system |
| records without `uuid` (titles, cost, mode, queue, api-request, …) | not turns | — |

- **User and assistant content** is converted per §4.
- **`toolUseResult`**, Claude's structured duplicate of a tool result, stays
  in the source.
- **Thinking signatures** stay in the source.
- **`attachment` and `system` records** are hashed as their payload minus the
  envelope fields (uuid, parents, session, timestamp, cwd, branch, version,
  entrypoint, userType, slug, isSidechain, agentId, requestId, promptId),
  wrapped as `x-claude-code:attachment` or `x-claude-code:system`.

**Human author.** Given at ingest (`--author`). When absent, human turns are
unattributed (`{"kind": "human"}`). It is never guessed from git config or
the environment, because it is part of every id.

**Verified properties**, checked on every session in the local corpus by
`test/test_corpus.py`:
- Ingest is idempotent.
- Every derived file reads back verified and byte-identical.
- Turn ids and heads are unchanged after a `restore`-style rewrite to another
  home, cwd and session id. The one exception is a transcript whose content
  already names the destination paths (§13.8).

## 9. Storage: envelope and content (not yet implemented)

Two object kinds, stored apart:

| object | holds | store |
| --- | --- | --- |
| envelope | turn cores, heads, signatures, pins | anywhere, including git. Immutable and small, and holds no transcript text. |
| content | content objects, raw source blobs | a store objects can be **deleted** from |

The bundle store chose S3 over git because a leaked secret in git can't be deleted. That
reasoning still holds for content and doesn't apply to envelopes. Deleting
content leaves a tombstone; the graph above it still verifies. Sharing a
conversation without some of its content is the same operation as deleting
it locally.

Intended layout, extending the existing store:

```
raw/<hex>        raw source file
content/<hex>    content object
turns/<hex>      turn core
```

## 10. Git pins (not yet implemented)

A pin links a turn to the commit the working tree was at:

```json
{"pin": "sha256:<turn>", "repo": "<remote url or null>", "commit": "<sha>"}
```

- **Pins are annotations, not part of the core.** Pinning a turn later
  doesn't change its id.
- **Each pinned commit is kept alive by a ref**
  (`refs/loch/pins/<turn-hex>`), so a squash or rebase can't garbage-collect
  the disk state a fork would check out.
- Forking at a turn is: check out its pin in a new worktree, emit the
  transcript up to that turn, resume.

## 11. Native turns (not yet implemented)

Turns that no agent transcript contains are first-class turns with no source:
- a second human's message
- a tool approval: an `approval` turn with a block such as
  `{"type": "approval", "call": "…", "decision": "allow"}`
- a merge: any kind with two or more parents, typically an agent-written
  summary message
- a notification ("Bob forked here"): a `note` turn

They are written by the tool that creates them as a source of their own in
loch format. Signatures are separate objects over a turn id and stay out of
the core, so a turn can gain signatures without changing identity.

## 12. Evolving

- **Converter version** bumps when derived output changes. Re-derive old
  files.
- **Format major version** (`loch`) bumps when the core changes meaning, and
  is the only change that changes ids.
- **Adding a block type, fact or header field** is not a version change.
  Readers ignore what they don't know.
- **A converter fix that changes core bytes changes ids.** Policy in §13.1.

## 13. Open questions

1. **Converter fixes vs stable ids.** Fixing how a record normalizes changes
   its turn id and every descendant's. Options:
   - freeze core-affecting converter behaviour per format major
   - emit `supersedes` records mapping old ids to new
   - accept churn until anything is shared

   Nothing is shared yet, so this is cheap to decide now and expensive later.
2. **Human identity.** What is an author `id`: email, SSH or GPG key
   fingerprint, a DID? Because the author is in the core, two people
   ingesting the same raw source with different `--author` get different
   ids. Is the ingester really the author of a Claude transcript's human
   turns?
3. **Tombstones aren't fully private.** A low-entropy content object ("yes",
   "lgtm") can be confirmed from its hash after deletion. Option: a salt
   inside the content object, derived deterministically per source so
   re-ingest stays idempotent.
4. **Floats.** Python's repr diverges from RFC 8785 for exponent forms
   (`1e+21` vs `1e21`). A second implementation in another language must
   match bit for bit. Adopt RFC 8785 exactly?
5. **Turn granularity.** One turn per record is faithful and keeps Claude's
   tree, but merge and review probably want message-level units. Should
   message-level grouping be a derived view, which is the v1 answer, or the
   identity unit?
6. **Should `context` and `event` turns be in the graph at all?** Hook
   summaries, attachments and reminders are Claude Code internals that shape
   the model's context. Today they are hashed turns (about 26% of all turns)
   carrying `x-claude-code:*` payloads, so their identity depends on Claude
   Code's internal record shapes.
7. **Sessions that moved directories.** Only the final cwd becomes
   `{{loch:root}}`. Earlier cwds normalize under `{{loch:home}}`, so a copy
   that moves the two directories differently won't match. Numbered roots?
8. **Content that names the destination.** A transcript that already
   contains the path it is later restored to is indistinguishable from a
   rewritten one. It happened once in 3,810 sessions, and that session was
   the one that built this test.
9. **Subagents.** Sidechain transcripts are hashed in the source but not
   converted. Natural shape: a child graph whose root's parent is the
   `tool-call` turn that spawned it.
10. **Spilled tool output** (`tool-results/`) is referenced by path from
    content. Inline it, or reference its source blob by hash?
11. **Images** are inline base64 inside content. Separate blob objects would
    dedupe and could be deleted independently.
12. **Raw snapshots.** Claude appends to live files, so "immutable raw source"
    means snapshotting at ingest. Should `ingest` write `raw/<hex>` to a
    store? Appending a session and re-ingesting keeps every earlier turn id
    (tested), so incremental ingest is safe.
13. **Pin capture.** Historical transcripts record a branch, never a commit.
    Capture pins live (a hook in the driving tool), infer them from
    `git commit` tool output, or both? And `refs/loch/pins/*` vs git notes.
14. **Envelopes in git** go against the bundle store.s "not git-backed" decision only
    in part: envelopes hold kinds, authors, hashes and structure, not text.
    Is structure alone sensitive enough to rule git out too?
15. **Conversation identity.** Heads name leaves, but what names "the
    conversation" across forks and machines — the root turn id, a
    conversation object, or nothing?
16. **Resume from a derived-only share.** Thinking signatures and
    `toolUseResult` stay in the raw source, so the emitter (loch → Claude
    Code) can resume natively only with the raw source at hand. Otherwise it
    synthesizes context.
