# loch format, version 1

**Status: draft.** The derived-file format and the Claude Code converter
(§3–§8) are implemented in `src/format.rs` and `src/ingest.rs`, and workspace
attribution (§10) in `src/snapshot.rs` and `src/attribution.rs`. The storage
and native-turn sections (§9, §11) describe intended shapes that nothing
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

- **Canonical JSON** is RFC 8785 (JCS), so a second implementation in any
  language reproduces an id byte for byte:
  - UTF-8 output
  - object keys sorted by UTF-16 code units
  - no whitespace
  - strings escaped as ECMAScript's `JSON.stringify` escapes them, including
    a lone surrogate as `\udXXX`, which real transcripts contain
  - floats printed by ECMAScript's Number-to-String (`1e+21`, `1e-7`,
    `0.00001`, `100`)
  - NaN and Infinity rejected

  **One departure:** integers print exactly at any size. JCS treats every
  number as a double, which would silently round a large integer inside a
  tool result.
- **Ids:** `sha256:` followed by 64 lowercase hex digits, the SHA-256 of the
  canonical JSON. The prefix names the algorithm so a future one can coexist.

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
| `kind` | `message`, `tool-result`, `context`, `compaction`, and reserved for native turns, `approval` and `note`. |
| `author` | `{"kind": "human" \| "agent" \| "tool" \| "system", ...}`. A human may carry `id`, normally `ssh:SHA256:<fingerprint>` (§13). Agent, tool and system authors carry `agent` (e.g. `claude-code`). Absent fields are omitted, never `null`. |
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
(normalized), `record` (the source record type), `subtype`, `danglingParent`,
and `events`: agent bookkeeping that followed this turn (§8).
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
outside the derived file (§13, conversation identity).

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
converted to turns (§13, subagents).

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
| other `system` (hook summaries, turn durations, API retries) | not a turn: folded into the preceding turn's `facts.events` | — |
| records without `uuid` (titles, cost, mode, queue, api-request, …) | not turns | — |

- **User and assistant content** is converted per §4.
- **`toolUseResult`**, Claude's structured duplicate of a tool result, stays
  in the source.
- **Thinking signatures** stay in the source.
- **`attachment` and `system` records** are hashed as their payload minus the
  envelope fields (uuid, parents, session, timestamp, cwd, branch, version,
  entrypoint, userType, slug, isSidechain, agentId, requestId, promptId),
  wrapped as `x-claude-code:attachment` or `x-claude-code:system`.
- **Folded events.** An event's uuid resolves to the turn it follows, so the
  record after it links straight past. Ids are therefore the same whether or
  not Claude Code wrote the event. An event with no turn before it goes into
  the header's `session.events`.

**Human author.** Given at ingest. `--author-key <key.pub>` gives
`ssh:SHA256:<fingerprint>`, the same string `ssh-keygen -l` prints, and
`--author <id>` takes any string. When neither is given, human turns are
unattributed (`{"kind": "human"}`). It is never guessed from git config or
the environment, because it is part of every id.

**Verified properties**, checked on every session in the local corpus by
`test/test_corpus.py`:
- Ingest is idempotent.
- Every derived file reads back verified and byte-identical.
- Turn ids and heads are unchanged after a `restore`-style rewrite to another
  home, cwd and session id. The one exception is a transcript whose content
  already names the destination paths (§14.1).

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
it locally. Withheld content is absent, not secret: a short message can be
confirmed by guessing it (§13, salting).

Intended layout, extending the existing store:

```
raw/<hex>        raw source file
content/<hex>    content object
turns/<hex>      turn core
```

## 10. Workspace attribution

Which turn wrote which lines of a project, recorded without git and found
again by content, so that squashing, rebasing, cherry-picking and cloning
lose nothing.

**Capture.** At the start and end of each turn the workspace is snapshotted.
The turn's changes are what differs between the two, however the agent made
them: an edit tool, a shell command, a generator, a formatter it ran.
- **The workspace root** is the nearest ancestor of the session's cwd holding a
  `.git` entry, read as a marker only, or the cwd itself where there is none.
  Symlinks are resolved first.
- **What counts as a file:** every regular file the user's ignore rules let
  through (`.gitignore`, nested ignores, the global excludes file), with or
  without a repository; `.git` itself is never one.
- **Roots that are refused:** `/`, the home directory, and any root holding
  more than 100,000 files.
- **Snapshots are content-addressed and local.** A snapshot's id is the
  SHA-256 of its manifest, one `<content sha256>\t<size>\t<path>` line per
  file, sorted by path.
- **Turn boundaries** come from the harness: Claude Code's `UserPromptSubmit`
  and `Stop` hooks, and `SessionEnd`. A harness with only an end-of-turn signal uses each
  turn's end snapshot as the next turn's start. With no signal at all, a
  change is attributed to the span between two snapshots.
- **Repeated stops are one turn.** Claude Code can stop more than once for one
  prompt; those stops are one turn, measured from where it began.
- **Every turn gets closed.** Claude Code runs no `Stop` for a cancelled turn
  and nothing at all for a crash. A turn still open when the session's next
  prompt arrives, or when the session ends, is closed then, marked
  `interrupted`, ending where the workspace stands at that moment; otherwise
  its edits would become the next turn's baseline and belong to nobody. When
  the next prompt's own record is already in the transcript, it is excluded
  from the interrupted turn. A crashed session that is never resumed is the
  one case nothing closes.

**What a turn added.** For each file whose content changed, a line diff
(Myers) from the start to the end snapshot gives the inserted and replaced
lines. Consecutive ones form a hunk. A new file contributes all of its
lines; a binary file (any NUL, or not UTF-8) and a deleted file contribute
none.

**Line hash.** The first 8 bytes of SHA-256 over the line with surrounding
whitespace removed, as 16 lowercase hex digits. Whitespace is excluded so a
formatter re-indenting a turn's code doesn't sever the link.

**Edit record.** One JSON document per session holds every turn's hunks:

```json
{"loch": 1, "agent": "claude-code", "session": "<id>",
 "project": "git:<root commit>", "root": "<workspace path on that machine>",
 "turns": [{"promptUuid": "<record>", "endUuid": "<record>",
            "started": "<ISO-8601>", "ended": "<ISO-8601>",
            "prompt": "<first 120 characters, redacted>",
            "interrupted": false,
            "files": [{"path": "<relative, / separated>",
                       "hunks": [{"start": 12, "lines": ["<line hash>", ...]}]}]}]}
```

- **What it carries:** hashes, plus the one field of transcript content `list`
  already carries (the prompt's first 120 characters, redacted). No code.
- **Where each turn begins and ends:** `promptUuid` and `endUuid` name the
  agent's own transcript records.
- **`project`** is the same on every clone and worktree. For a git repository
  it is `git:` plus the repository's root commit (the smallest, if there are
  several): read once, and immune to anything later history does. Anywhere
  else it is `dir:` plus the root's directory name, which is weaker.

**Store layout:**

```
edits/<agent>/<session>.json                       the record, rewritten each turn
index/file/<percent-encoded project:path>/<session> derived: which sessions touched a file
```

The index is derived, like every index entry (§9's rules for the store
apply): `reindex` rebuilds it from the records.

**Matching.** To attribute a file's current lines:
1. **Gather candidates.** Every turn of every record that changed this
   project-relative path.
2. **Hash** each current line as above.
3. **Claim runs.** A hunk claims a maximal run of the current file's
   consecutive lines whose hashes equal consecutive hunk lines, provided at
   least one line in the run is not *weak*. A weak line has fewer than three
   alphanumeric characters: a brace, a blank, `end`. A run is what
   attributes; one `}` matching proves nothing.
4. **Apply oldest first.** Turns are applied in order of `ended`, so the most
   recent turn to write a line's text owns it.

A line no hunk claims was not written by a recorded turn. That includes a
line someone rewrote after the agent wrote it, which is the intended answer.

**Snapshots are disposable.** Nothing reads a snapshot once its turn has
been diffed, since blame works from the hashes in the records alone, so a
collector keeps only what a future diff needs. Mark and sweep, from two kinds
of root:
- **each workspace's latest snapshot,** the next turn's baseline;
- **each live session's turn start.** A session's state untouched for 7 days
  belongs to a crash nobody resumed, and stops being a root.

Everything else written more than an hour ago is deleted. The hour covers
another session's hook that has snapshotted but not yet recorded the id. The
collector runs from the hooks once 50 snapshots have accumulated, as
`git gc --auto` does, and never in the background.

`ask` resumes a conversation against today's files, not the files as they
stood when the turn ended; restoring those was considered and isn't wanted.

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

- **Until the first conversation is shared, everything here is draft.** Ids
  may change from one commit to the next, and nothing needs migrating,
  because nothing exists outside one machine and every derived file can be
  regenerated.
- **After that:**
  - The **converter version** bumps whenever derived output changes. Old
    derived files are re-derived, never migrated.
  - A converter change that alters **core bytes**, and so ids, also bumps
    the **format major version** (`loch`). It ships a `supersedes` map from
    old ids to new, so anyone holding the old ids can follow them.
  - **Adding** a block type, fact or header field is not a version change.
    Readers ignore what they don't know.

## 13. Decisions

These were open in the first draft. Each is settled, with the reason.

| question | answer | why |
| --- | --- | --- |
| Number encoding | RFC 8785, with exact integers (§3) | A second implementation must reproduce ids byte for byte. |
| Converter fixes vs stable ids | Free churn until first share, then version bump plus `supersedes` (§12) | Costs nothing now, and keeps ids followable later. |
| Turn granularity | One turn per agent record. Grouping into whole replies is a view built from `facts.messageId`. | The only option that keeps the agent's own tree, rewind branches included. |
| Agent bookkeeping in the graph | `context` stays a turn. `event` is folded into facts (§8). | Context shaped what the model saw. Events are an agent's internal records, and as turns they made about 2% of turns, and every id after them, depend on its internal shapes. |
| Human identity | `ssh:SHA256:<fingerprint>` | Same as git's SSH signing: no new infrastructure, can be checked against keys people already publish, and the same key can sign turns later. |
| Who authors a transcript's human turns | Whoever ingests it | In practice the person who ran the session. Everyone else receives their derived file and doesn't re-ingest it. |
| Conversation identity | Its root turn id | Forks share the root, so they stay recognizably the same conversation. A merge has several. Heads are local names, like git branches. |
| Sessions that moved directories | Numbered roots (`{{loch:root:1}}`, …) for every cwd the session held, in order of first appearance | The current single root lets a copy that moves only one directory change ids. Not yet built. |
| Subagents | A child graph whose root's parent is the `tool-call` turn that spawned it | Keeps a subagent's turns reachable from the conversation that asked for them. Not yet built. |
| Spilled tool output and images | Separate content objects referenced by hash, rather than inline | They dedupe, and can be deleted on their own. Not yet built. |
| Raw snapshots | `ingest` writes the raw source to `raw/<hex>` in a store | Claude Code appends to live files. Re-ingesting a grown session keeps every earlier turn id (tested), so snapshots can be incremental. Not yet built. |
| Pin capture | Live, from the driving tool. Old transcripts get none and are not guessed. | A pin inferred from tool output that turns out wrong is worse than no pin. |
| Envelopes in git | Yes | They carry structure and hashes, never text. Deletability applies to content (§9). |
| Resume from a derived-only share | The emitter synthesizes context | Thinking signatures and `toolUseResult` exist only in the raw source. With it at hand, the emitter resumes natively. |
| Deleting content | A tombstone: the content object goes, every id stays. Not git's rule, where a missing object is corruption and removal means rewriting history. | The structure is git's (a turn holds its content's hash as a commit holds a tree's), so content still can't change without its ids changing. Only the presence rule differs. A removal is visible either way, as a hole rather than new ids, but a tombstone doesn't force a rewrite on everyone downstream, which a leaked secret shouldn't need. Replacing content rather than removing it is a fork, with new ids. A reader that wants git's guarantee can refuse incomplete conversations; nothing implements that check yet. |
| Salting content hashes | No salt. Content ids are the plain hash of the content object. | Keeping content private is not a goal. Deleting content exists to remove things like a leaked secret, which is too high-entropy to recover by guessing. A short deleted message ("yes", "lgtm") can be confirmed by hashing guesses against its envelope; that is accepted. Chaining wouldn't prevent it, since the guesser holds the parents too. |

## 14. Known limits

1. **Content that names the destination.** A known limit rather than a
   question. A transcript that already contains the path it is later
   restored to can't be told apart from a rewritten one. It happened once in
   3,810 sessions, in the session that built this test.
