# loch — repo overview for Claude

## What this is

A standalone CLI that makes agent coding sessions portable between
machines. Agent CLIs store transcripts in machine-local directories keyed
by the working directory they ran in, which strands a session on the
laptop that produced it. `loch` packs those transcripts into a
content-addressed bundle, stores it anywhere S3-compatible, and unpacks
it elsewhere with paths rewritten so the session actually resumes.

Supports Claude Code and Codex, behind one adapter boundary (`agent.rs`,
below). Cursor is the obvious next one.

It is also becoming the **conversation format**: `loch ingest` turns a
session into a content-addressed graph of turns that can be forked, shared
and eventually merged between people. loch is the `git` of that world and an
editor (Ness first) is the `vim`. The format is specified in `SPEC.md`, which
is the one design document here on purpose: it is meant to be implementable
by someone who never reads this file.

## Why it exists

The driving use case is **PR review**: a reviewer should be able to pull
down the sessions that produced a branch, read them, and *talk to them* —
ask the agent that wrote the code why it made a choice, with all its
original context intact. That last part is why plain transcript viewing
isn't enough and `restore` has to actually work.

The longer-term goal is prompts/sessions being first-class artifacts that
travel with the code they produced.

## Deliberately not part of this project

- **Not a Harness feature.** Harness is one consumer among many. Anything
  that only makes sense inside Harness belongs in Harness.
- **Not a normalized source of truth.** Resume is same-agent only — a
  Claude transcript can't be resumed by Codex, the tool-call schemas differ.
  Native blobs round-trip; normalization would be lossy for the one thing
  that matters. The loch format *is* a normalized projection, and that is
  exactly its status: derived, stamped with the converter that made it,
  regenerated from the raw source whenever the converter improves. The raw
  transcript stays the thing that has to survive.
- **Not git-backed storage for content.** See below. Turn envelopes (which
  hold hashes and structure, never transcript text) are a different
  question — SPEC.md §9 and §13.

## Design decisions and their reasoning

**Object storage, not git.** The deciding factor was deletability, not
size. Transcripts capture verbatim tool output, so a session may need
purging later. Git's content-addressed immutability is a feature for code
and a bug for logs — a leaked key in `refs/sessions/*` means
`git filter-repo` across every clone. S3 lets you `DELETE`. Targeting the
S3 *API* (not AWS) means R2, B2, MinIO, and Garage all work, and users
can self-host. The local-filesystem backend behind the same interface
covers the no-infra case.

**Content-addressed refs.** The ref is the SHA-256 of the packed bytes,
so the storage URI is just where to fetch from. Swapping backends — or
migrating to git/LFS if object storage ever proves onerous — doesn't
change the identity of a bundle.

**Bundles hold N sessions.** A branch usually has several sessions, and
a reviewer wants one ref to pull. `save` filters by cwd/branch/session
and packs everything matching.

**Link at branch level, not per commit — for bundles.** A reviewer asks
"what produced this PR," not "what produced commit abc123," so bundles are
filed by branch. Nothing writes git pointers for bundles.

**Attribution by content, not by commit.** Which turn wrote a line is
recorded by snapshotting the workspace at each turn's start and end and
hashing the lines the turn added (`attribution.rs`, SPEC.md §10), never in
git. The first design put a git note on each commit an agent made, and a
squash merge destroyed every one of them: a note annotates one commit id, and
a squash makes a new commit out of several, so #2's notes ended up on commits
`main` doesn't contain. Rebases, cherry-picks and amends do the same thing
more quietly. What should be remembered is "this text was written by this
turn," which no history rewrite changes, so that is what is recorded, and the
records live in the store, which also keeps them deletable (above). Four
requirements fixed the rest of the design:

- **Every edit counts, however it was made.** More than half of an agent's
  edits go through the shell (`sed`, generators, formatters, scripts), and the
  harness may have no edit tool to observe. So capture looks at the files, not
  the tools: anything that lands on disk during a turn is the turn's.
- **Per turn, not per tool call.** A turn is the unit anyone asks about, and
  per-turn boundaries are available from far more harnesses (a turn-end hook,
  or transcript timestamps) than per-call ones. Claude Code's
  `UserPromptSubmit` and `Stop` hooks are the two moments needed. The old
  pre/post-tool hooks, which ran twice per shell command, are gone, and
  `install` removes them.
- **Every turn gets closed by someone.** Claude Code runs no `Stop` for a
  cancelled turn and nothing for a crash, so the cancelled turn's edits would
  become the next turn's baseline and belong to nobody. A session's state
  therefore says whether its turn is open, and the next `prompt` — or
  `SessionEnd`, the third hook — closes it first, marked `interrupted`. Every
  state and record file is written aside and renamed, so a hook killed
  mid-write leaves the old file or the new one, never half of either.
- **Hooks never touch the network.** They write the edit record, its file
  index and a copy of the transcript to the local store (`~/.loch/store`,
  `store::local_store`), whatever `LOCH_STORE` says, and so do `backfill`'s
  records. `blame`, `log` and `ask` read only that store. Publishing from the
  hook was the first design, and a remote that didn't answer held every
  `Stop` for the S3 timeout (`end` took 8.3s against an unreachable bucket),
  while Codex kills a `SessionEnd` hook at 3s, so the last turn's record
  never left the machine. Getting records to and from a remote is an explicit
  step, like git's push and fetch. `hooks_never_wait_on_a_remote` pins it.
- **No background process.** Snapshots are taken on demand with a stat cache,
  the way git's index works: files whose size and mtime are unchanged aren't
  read again. A harness with no hooks gets coarser attribution, never a
  daemon.
- **A person editing during a turn is not a concern.** Their changes in that
  window are the turn's. Changes between turns are not, provided the prompt
  hook ran.

**`install` is user-wide by default, and a turn is recorded once.** Three
scopes (`agent::Scope`): the user's own hooks file (the default; Claude Code's
`settings.json` under `claude.config_dir`, Codex's `$CODEX_HOME/hooks.json`),
`--local` (this project on this machine: `.claude/settings.local.json`) and
`--project` (the committed `.claude/settings.json`, the old default, kept
because it is the only way to make recording travel with a repository to
teammates). Codex has no uncommitted project file, so its `--local` and
`--project` are both `.codex/hooks.json`. Codex reads that file only in a
project trusted in `config.toml` (`[projects."<path>"] trust_level =
"trusted"`), on top of the per-hook trust (verified on 0.166 in a scratch
`CODEX_HOME`: an untrusted project's file never fired, the user's did).

- **A user-wide hook records only in a git repository.** It runs in every
  directory an agent does, and the fallback that makes a plain directory a
  workspace would otherwise snapshot `~/Downloads`. A repository with no
  commits is still recorded (its project id is `dir:<name>`). `hook::applies`
  decides this from the event's cwd before a store is opened, so outside a
  repository the hook costs one walk up for `.git`. A project-level install
  still records a plain directory, since it was asked for there.
- **Each event is handled once, however many of loch's hooks it reaches.**
  Claude Code and Codex run every scope's hooks for an event, at once, so a
  user-wide hook and a project's own (or an old committed line beside Ness's
  local one) would both snapshot a turn and race over its state, the second
  `prompt` closing the first's turn as an empty interruption. Each event's
  hooks get byte-identical stdin, so `hook::claim` takes a per-session lock
  (`~/.loch/events/<session>.last`), and a hook handed the same event that
  another finished handling within 2s (timed in milliseconds) does nothing. An event is the hook's
  name, its payload and its transcript's length. The payload alone can
  repeat: Claude Code stopping twice for one prompt sends identical bytes,
  which the first version of this took for a duplicate, losing the second
  stop's edits (`stopping_twice_for_one_prompt_is_one_turn` caught it). The
  agent waits on these hooks, so every hook for an event sees the same
  transcript, and a genuine second event follows output that grew it.
  `UserPromptSubmit` also carries a `prompt_id`, and Codex's events a
  `turn_id`. The window is narrow because duplicates read the claim within
  milliseconds of its being written, while a genuine repeat that hashes the
  same (a second `Stop` before the transcript is flushed) follows a model
  round trip. A wider window could only drop such an event. A lock that can't be taken
  handles the event anyway, since a duplicate record beats a lost turn.
- **The first design guessed, and the guess could lose turns.** It had a
  wider scope's hook step aside when a narrower file held loch's hook "and
  the agent will run it". Answering that means knowing where the agent reads
  a project's settings, whether it trusts them, and whether they changed
  after it started (both agents snapshot hooks at startup). For Codex it
  meant reading trust keys per group and index out of `config.toml`. Each
  wrong answer skipped a hook the agent never ran, so nobody recorded the
  turn. Idempotency needs none of those answers.
- **What was verified, on Claude Code 2.1.329 and Codex 0.166** (scratch
  `HOME`/`CODEX_HOME`; Claude ran `UserPromptSubmit` before failing on its
  missing login, which was enough): two user hooks and a project-local hook
  for one event got byte-identical payloads, for both agents.
  `UserPromptSubmit`'s keys are `session_id`, `transcript_path`, `cwd`,
  `permission_mode`, `hook_event_name`, `prompt` and `prompt_id`. Claude
  Code launched in `repo/sub` ran the repo root's `.claude/settings.local.json`,
  and gave hooks `CLAUDE_PROJECT_DIR` as `repo/sub`. Claude's `Stop` payload
  was not captured, since that needs a model call. If it ever differs
  between hooks, the cost is a duplicate record.
- **Only loch's own lines are touched.** `trace::parse_hook_line` recognises a
  line `hook_line` wrote, whatever the path to loch. The first loch line for
  an event in the target file is rewritten in place if it differs (another
  path, another scope) and reported as `replaced`. A second one, which an
  older install appended after loch moved, is removed for Claude Code
  (`removed`). For Codex it is left where it is and reported as
  `duplicates`, since removing a group shifts the positional trust keys of
  every group after it, and the claim makes it harmless. Only user-wide lines
  name a scope (`--scope user`, the one `hook::applies` acts on), so a
  project's committed lines and Ness's local ones are what they always were.
- **The user's file names plain `loch`.** That survives an npm upgrade under
  another Node version, at the price of needing loch on the agent's PATH.
  `--local` still defaults to the absolute path.

Git is still *read*, never written: for a project's identity (its root
commit, the same on every clone and immune to history rewrites), for `list`'s
default branch, for a file's earlier names (`git log --follow`, so blame finds
turns that wrote it before a rename), and by `backfill`.

**History from before the hooks is backfilled, honestly labelled.**
`loch backfill` rebuilds turns from what survives, marked `backfilled`:
- **The transcripts' `Edit`/`Write`/`MultiEdit` calls,** which carry the exact
  text written. Failed edits are skipped. Fragment edits (part of a line)
  can't match a full line and are missed.
- **The commits the agent made,** whose diffs catch shell edits. A commit is
  credited only on an exact match: an old git note naming the call, or
  exactly one Bash call — across every session — containing the commit's
  subject and bracketing its time. A contested commit stays unlinked, since a
  wrong link looks exactly like a right one. A commit's lines that the same
  session's tool edits already explain are left to those edits, the more
  precise source.

- **Every worktree of the repository, live or removed.** Most agent work
  happens in a worktree that is merged and deleted, so a backfill from any
  checkout covers sessions whose cwd was in any worktree `git worktree list`
  names, each edit's path made relative to the worktree it was written in.
  A removed worktree is recognized **by what it wrote, not where it was**:
  worktrees live wherever their creator put them (Ness nests them by branch
  name, Claude Code under `.claude/worktrees`, people under `/tmp`), so no
  layout is assumed. Each component prefix of the gone cwd is a candidate
  root, and wins if the session's substantive lines (12+ characters, not
  weak) are in this repository's files at the same relative paths, at the
  tip of any branch, tag or worktree — at least 5 distinct lines, and
  strictly more than any other prefix. A tie, or less, includes nothing. A
  cwd inside another repository's checkout is never considered, whatever it
  wrote, and nor is a directory that still exists but is no checkout of this
  repository (scratch space, a `cp -r`): only a directory that is gone can
  be a worktree git has forgotten. Blobs over 1MB are not read for evidence
  and the rest are read 16MB at a time, so a lockfile at hundreds of tips
  never sits in memory whole. The repository's tips are read as one tree walk per depth through
  `cat-file --batch`, since hundreds of branches share nearly every subtree
  (claude-harness: 666 tips, 7s by `ls-tree` per tip, 0.3s this way), and a
  transcript is fully parsed only if a `file_path` in its raw text could land
  on a path the repository has. On claude-harness this took backfill from 17
  turns to 185 across 45 sessions in 39 worktrees, and `src/main/loch.ts`,
  written in a since-removed worktree, from 0 to 383 of 439 lines.
  The honest limits: a removed worktree whose work never reached any ref
  leaves no evidence (nor anything to blame); a session in a removed
  worktree that worked only through Bash has no tool edits to be recognized
  by, so it is skipped with its commits even when they are still reachable
  (recognizing a gone cwd by an exactly-credited commit would fix that, and
  is exact by construction); and a squash-merged commit
  can't be credited by the commit rule, since its time is the merge's, not
  the Bash call's — its lines come from the tool edits or not at all.

Hook-recorded turns are never replaced, and a re-run replaces only earlier
backfilled turns. On this repository it rebuilt 21 turns across 2 sessions,
and with rename-following, files written before the crate moved out of
`rust/` went from 1–16% attributed to 96–99%.

**Snapshots are disposable, so they are collected.** Nothing reads a snapshot
once its turn is diffed; blame works from the records' hashes. A
mark-and-sweep keeps each workspace's latest snapshot and each live session's
turn start, and deletes whatever else is more than an hour old. It runs from
the hooks every 50 snapshots, like `git gc --auto`, or as `loch gc`. Steady
state is about one compressed copy of each project plus the turns in flight.
`ask` resumes the conversation against today's files; restoring a turn's
exact files was considered and isn't wanted.

**No single index object.** A shared `index.json` would need
read-modify-write on every save, so two machines saving concurrently would
silently clobber each other. Every index write is instead a new object at
a distinct key, which makes concurrent saves commutative and needs no
locking from a store that offers none.

**Index entries are derived, never authoritative.** The bundles are the
source of truth and every entry is reconstructible by listing `bundles/`
and unpacking. That is what makes `reindex` a repair command and what
makes a half-failed `save` leave a recoverable store rather than a corrupt
one. Nothing may end up knowable only from the index.

**Push and pull, git style: `save` is `commit`.** The local store
(`~/.loch/store`) is where everything is recorded first, bundles included.
Named remotes (`loch remote`, kept in `~/.loch/config`) are where `push` sends and `pull` fetches from, as git's
remotes are. `blame`, `log` and `ask` read only the local store, so a
teammate's turns arrive by `pull`, never by a read that waits on the network.
- **What moves.** `push` sends the current project's bundles with their
  branch entries, and its sessions' edit records, file index and transcripts
  (`--all` for every project's). A bundle belongs to a project if any session
  in it ran there: sending every bundle on the machine, as the first cut did,
  put one client's transcripts on another's remote. A bundle whose sessions'
  directories are gone from this machine can't be placed and goes only with
  `--all`. Bundles were redacted by `save` and go as they are, so a ref is the
  same everywhere. Transcripts are kept verbatim locally and redacted on the
  way out. One in which a secret survives redaction is skipped and named in
  `redactionFailed`, the rest still goes, and the push ends `redaction-failed`.
  Redaction is deterministic, so failing the whole push would block every
  later one. Edit records hold line hashes, and their `prompt` was redacted
  when recorded. `pull` fetches records with their transcripts, scoped through
  the remote's file index, so a session whose turns edited no file isn't
  pulled; it has nothing to blame anyway. Bundles are not pulled.
- **Edit records merge, they are never replaced.** A session restored under
  its own id and resumed is recorded by two machines, and a record is one
  mutable object per session. Writing it whole erased the other machine's
  turns, locally (the hook started from its working copy alone, which `pull`
  never fills) and on the remote (the next push). So `EditRecord::merge`
  takes both copies' turns, a turn known by its prompt's id (else by when it
  ran), the later stop winning. The hook merges the working copy with the
  store's, `pull` merges into the local copy, and `push` merges the remote's
  copy in before it uploads and keeps the merge here. Two pushes of one session
  at the same moment can still drop one's new turns from the remote, until
  that machine's record next changes and its push merges them back. The
  other side of merging: a turn removed on one machine (a `backfill` re-run
  dropping a contested backfilled turn) comes back from any other copy, and
  only a tombstone removes anything for good. That is the trade taken, since
  a lost turn is silent and a resurrected one is visible.
  Transcripts are still one object per key, last writer wins. Claude's keys
  name the project directory, so a session resumed in another checkout gets
  its own key, but a Codex rollout's name doesn't change on restore.
- **What has been sent is remembered here, not there.** `~/.loch/pushed/<uri
  hash>.json` maps each key to the hash of the local bytes last sent, so the
  remote stays a set of independent objects (no manifest, per "No single
  index object") and a second push sends only what changed. Losing that file
  only re-sends. A stale one is the silent case: a remote emptied or replaced
  by anything but `delete --remote` makes every push say it is up to date.
  `push --force` ignores the markers. Push sends a bundle before its entries
  and a record before its file index, the order `save` and the hooks keep, so
  a push cut short leaves what `reindex` can repair.
- **A delete on a remote stays deleted.** A tombstone (`tombstones/bundle/
  <ref>`, `tombstones/session/<agent>/<id>`) is one new key per delete and
  holds no content. Every push reads them first and sends nothing they cover,
  so a teammate's copy can't bring a deleted session back. It reads them
  again afterwards and withdraws what a delete racing it covered, which
  closes the race in practice but not atomically. Every pull copies them here
  and forgets loch's copies of what they cover, but never an agent's
  transcript in its own directory. Being able to delete is why loch uses
  object storage, so this is stricter than git on purpose. Whether a local
  tombstone should hold for every remote is to be tested against use.
- **The store commands, by git's answer.** `save` writes the bundle here,
  and `save --push [<remote>]` sends that bundle alone afterwards. A push
  failing after the save is reported with the ref, since the bundle exists.
  `restore` looks here first, then fetches the one bundle from the remote
  (`fetchedFrom`), checks that its bytes hash to the ref, and keeps it, as
  git keeps what it fetched. Content addressing is what makes any remote's
  copy as good as another's. `list --saved` is this machine's bundles,
  `list --remote` is `ls-remote`, and plain `list` is still the live
  sessions. `delete <ref>` is local. With `--remote` it writes the tombstone
  on the remote first (so a racing push sees it), deletes there, and
  tombstones and deletes here too. `delete --session <id>` does the same for
  a session's record, file index and transcripts, which is the delete that
  matters, since transcripts are what leak. A ref in neither place is
  `bundle-not-found`, and no tombstone is written for a typo. `reindex` is
  per store. `--store <uri>` still works everywhere, meaning `--remote <uri>`
  (`save --push <uri>` for `save`), which is the meaning callers had.
- **The one silent change, made loud.** A caller with `LOCH_STORE` set that
  runs a plain `save` used to upload. Now it gets `ok: true` and a bundle on
  this machine only, so it is warned (stderr in text, `"deprecated":
  "LOCH_STORE"` in JSON), and `schema` is 3. The Harness "send worktree to
  remote" flow needs `save --push` (or a push to the destination as a
  remote), and `restore` there finds the bundle by fetching it.
- **A remote's keys are someone else's input.** `FileStore` refuses a key
  with an empty, `.` or `..` segment, and `pull` takes a record only for an
  agent this build knows, an id `is_safe_session_id` accepts, and contents
  that name the session and project its key does. Anything else is counted
  as `refused` and never written.
- **The remote is never guessed.** A command names a remote or a URI, else
  `LOCH_REMOTE`, else `LOCH_STORE` (its old meaning, with a warning on
  stderr in text mode and `"deprecated": "LOCH_STORE"` in JSON), else
  `origin`, else the only remote. Several remotes and no `origin` is
  `ambiguous-remote`, since a push to the wrong one publishes transcripts. A
  URI scheme the store doesn't know is `invalid-uri`: `create_store` reads
  anything else as a relative path, so `ssh://host` would have become a
  local directory a push succeeded into. The local store itself is refused
  as a remote, compared resolved, so a symlink to it is refused too.

## Empirical findings about Claude Code's storage

These were verified by experiment, not documentation, and are not stable
public API. Re-verify if something breaks.

- Sessions live at
  `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`, or under
  `$CLAUDE_CONFIG_DIR/projects` when that is set (how one machine runs
  several accounts). loch follows the same rule and only that rule —
  never both places — since a restore filed anywhere Claude doesn't look
  is a session `--resume` can't find. `claude.config_dir` is the one place
  it's decided.
- The encoding is `re.sub(r"[^a-zA-Z0-9]", "-", cwd)` — every
  non-alphanumeric character, not just slashes.
- The cwd is **symlink-resolved first**, so `/tmp/x` is stored as
  `-private-tmp-x` on macOS. Missing this breaks lookups.
- Records carry `cwd`, `gitBranch`, `sessionId`, `version`, and a
  `parentUuid` chain. `gitBranch` on nearly every line is what makes
  branch filtering possible without any hook instrumentation.
- **The `sessionId` field on every line must agree with the filename.**
  On `--resume` the CLI appends lines stamped with the filename's id, so
  a file whose existing lines disagree becomes a hybrid transcript. When
  minting a new id, rewrite both.
- Absolute paths are pervasive: a real session had 1440 occurrences of
  the origin home directory across 64% of its lines, inside tool inputs
  and results. Rewriting is a whole-transcript operation.
- **A session can hold more than one `cwd`, and more than one
  `gitBranch`.** When the agent moves — into a worktree, say — the cwd
  changes and every record from before the move stays in the file. The
  transcript is filed under the directory it *ends* in, so the first
  record's cwd can be stale, and stale by a long way: in the session this
  was found on, records 5–295 named the main repo on `main` and records
  300–1245 named the worktree on `feat/send-worktree-to-remote`. The runs
  are contiguous and the live one is last.
- `encode_path` of the live cwd equals the containing directory's name
  exactly, and the stale one's does not. **The directory is the
  disambiguator**, and the only one available — no field marks which cwd
  is current.
- `"cwd"` also appears *inside* tool inputs and results, naming some
  other process's directory. loch's own `--json` output is one such
  source, so anything counting cwds has to parse each record and read the
  top-level field, not grep the text.
- Scale reference: 750MB across 428 project dirs on one active machine;
  largest single session 3.4MB. Per-branch bundles are a few MB gzipped.
  Parsing every line of that 3.4MB session costs ~11ms and listing its
  whole project directory (7 sessions) ~17ms.
- **Multi-cwd sessions are rare and concentrated where it hurts.** Across
  661 sessions belonging to one machine's repos, 7 (1.1%) held more than
  one cwd and exactly 1 had its live cwd past line 200. That one is the
  longest and most-compacted session in the set — 1245 lines, several
  context compactions — and it is the compaction preamble that pushes the
  real cwd down the file. So the defect concentrates in long-lived
  sessions, which are precisely the ones worth moving between machines,
  and it grows as sessions get longer. That distribution is the argument
  for the project-directory disambiguator over any larger window: a
  bigger window is a bet on a bound that the worst cases are the likeliest
  to break.

## Empirical findings about Codex's storage

Verified on codex-cli 0.166.0 by experiment (scratch repositories, real
`codex exec` and TUI runs), against the 70 rollouts on one machine
(versions 0.46, 0.121, 0.165, 0.166). Not stable public API: re-verify if
something breaks. Claude's rules above do **not** carry over; each was
re-derived.

- **Where sessions live:** `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<local
  time>-<id>.jsonl`, `$CODEX_HOME` defaulting to `~/.codex`. Filed by date,
  not by directory, so there is no directory to look in and discovery reads
  every rollout (`codex.rs`). That costs ~50ms for 70 rollouts (10MB) and is
  linear in total bytes.
- **`codex resume <id>` needs the rollout file and nothing else.** A copy
  under a fresh id, with no row in any sqlite file, resumed and recalled its
  conversation, and Codex inserted the `threads` row itself afterwards. The
  sqlite files are projections: `state_5.sqlite`'s `threads` holds
  `rollout_path` and summary columns, and `thread_history_1.sqlite` is keyed
  by rollout byte offset (`thread_history_projection_state`). Lookup is by
  the id the file name ends with. The date directory and the time in the
  name don't matter (a copy under `2025/03/01` resumed), so `restore` files
  a session under the day its `session_meta` says it began.
  `session_index.jsonl` holds thread names alone (appended, last wins), so
  loch reads it for blame's title and never writes it.
- **Records** are `{"timestamp", "type", "payload"}`, newer ones with an
  `ordinal`. `session_meta` comes first: `id` and `session_id`, `cwd`,
  `cli_version`, `originator`, `git: {commit_hash, branch}`,
  `history_mode` (`paginated` from 0.165, absent before). The id also
  appears in `thread_settings_applied.thread_id`, `item_completed.thread_id`
  and `token_usage_record.session_id`, so, as with Claude, minting an id
  means rewriting every occurrence (the rewrite's session pair does), and
  the file name too.
- **A turn is named by id.** `task_started`, `turn_context`,
  `item_completed`, `token_usage_record` and `task_complete` carry
  `payload.turn_id`, and response items carry it in
  `internal_chat_message_metadata_passthrough.turn_id`. But a `resume`
  writes `thread_settings_applied`, which names no turn, *before* the next
  `task_started`, so "the last turn seen" puts it in the previous turn. A
  turn therefore runs from the first to the last record **naming** it,
  which is where `ask --at-turn` cuts.
- **What a person typed** is `event_msg` `item_completed` with a
  `UserMessage` item (0.165+), or `event_msg` `user_message` (older).
  `response_item` user messages are not it: Codex injects
  `<environment_context>` and AGENTS.md as user messages.
- **The live cwd is the last `turn_context`'s.** `resume -C <elsewhere>`
  appends a `turn_context` naming the new directory, and `session_meta`
  keeps naming where the session began. Nothing else marks which is live,
  and there is no directory-name disambiguator as there is for Claude. The
  last turn is where the session most recently ran, which is what a
  restore wants. Cwds are counted from `session_meta` and `turn_context`
  only. `world_state` and tool calls name directories too, so a substring
  count would cry wolf, as it would for Claude.
- **Codex does not resolve symlinks in `-C`.** A session begun in `/tmp/x`
  recorded `/private/tmp/x` (the process's resolved cwd), and the same
  session resumed with `-C /tmp/y` recorded `/tmp/y`. Discovery compares
  cwds resolved.
- **The branch is recorded once,** in `session_meta.git.branch`, for the
  directory the session began in. No turn records one, and neither does
  `world_state`. A moved session's branch is unknowable from its rollout, so
  it is reported as none (`[no branch]`, `%00` in the index) rather than
  the old directory's, which is Claude's stale-branch defect. A session
  begun through a symlink to where it is now has not moved. None of the 70
  local rollouts had moved, so this is the rare case, and it is the honest
  answer.
- **Hooks** use Claude's file shape,
  `{"hooks": {"<Event>": [{"hooks": [{"type": "command", "command", "timeout"}]}]}}`,
  from `$CODEX_HOME/hooks.json` and `<project>/.codex/hooks.json` (plus
  plugins and config layers). 0.166 knows `PreToolUse`,
  `PermissionRequest`, `PostToolUse`, `PreCompact`, `PostCompact`,
  `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `SubagentStart`,
  `SubagentStop`, `Stop`, `Interrupt` and `PreModelRequest`. They're enabled
  by `[features] hooks` (stable and on here; `codex_hooks` is its deprecated
  alias). It clamps the `SessionEnd` and `Interrupt` timeouts to 3s. Captured
  payloads: every event carries `session_id`, `transcript_path`, `cwd` and
  `hook_event_name`. `UserPromptSubmit` and `Stop` add `turn_id`, `model`
  and `permission_mode`; `UserPromptSubmit` adds `prompt`, and `Stop` adds
  `stop_hook_active` and `last_assistant_message`. `SessionEnd` adds
  `reason` and **no `turn_id`**. So `hook.rs` is unchanged; only reading the
  turn back from the transcript differs.
- **New hooks don't run until a person trusts them.** Trust lives in
  `config.toml` as `[hooks.state."<file>:<event_snake>:<group>:<index>"]
  trusted_hash = "sha256:…"`. An untrusted hook is skipped silently in
  `exec`, and the TUI opens with "Hooks need review: N hooks are new or
  changed" (*Review hooks* / *Trust all and continue* / *Continue without
  trusting*), with `/hooks` to manage them. The hash is not a plain SHA-256
  of the entry. **loch never writes trust**: it is the user's review of a
  command that runs outside the sandbox, and forging it would defeat the
  point. `install` reports `needsReview` and says to open `/hooks`. Trust is
  keyed by position, so loch appends its hooks as new groups and leaves
  every existing entry's position, and so its trust, alone. That is what
  keeps Ness's user-level hooks trusted after a user-wide install.
  `--dangerously-bypass-hook-trust` exists for automation. The tests used
  it, and loch never passes it.
- **A cancelled turn gets no `Stop`.** Esc mid-turn writes `turn_aborted`
  (`reason: "interrupted"`) and runs no `Stop`, exactly like Claude Code,
  so the next prompt's hook closes it as `interrupted`, keeping its edits.
  Verified in the TUI. `SessionEnd` runs on `/quit` and at the end of every
  `exec`, so quitting mid-turn is closed the same way. The `Interrupt` event
  would close a cancelled turn sooner, but no payload was captured and
  nothing needs it, so it isn't installed.
- **`codex fork` can't cut at a turn** (no `--at-turn`), and needs a session
  Codex can already find. `ask` writes its own fork instead: a prefix of the
  rollout's lines, kept byte for byte, under a new id, rewritten into the
  target cwd. It resumes with `codex resume <id>`. It has no title, since
  titles live in `session_index.jsonl`.
- **Tool calls changed shape across versions:** 0.166's are
  `custom_tool_call`s named `exec` whose input is JavaScript calling
  `tools.exec_command`, and older ones are `function_call`s. So backfill's
  approach (the text an `Edit`/`Write` call carries) doesn't carry over, and
  `backfill` stays Claude-only.

## The adapter boundary, and what a third harness costs

`agent.rs` is everything loch needs from one harness, in three tiers. Each
later tier's methods have defaults that decline, so an adapter is as big as
what the harness supports:

1. **Attribution** (`blame`, `log`). Required: an `id`, the hook events
   (a turn's start, its end, and ideally the session's end) and the hooks
   file `install` writes. That is the whole requirement, because snapshots,
   diffs, edit records and matching are harness-agnostic, and `hook.rs`
   only needs a payload with `session_id` and `cwd` (Claude-shaped field
   names; an adapter for differently named fields would add a payload
   mapping). Optional: `current_turn` (which transcript record a turn began
   and ended at, and its prompt) and `transcript_key` (keeping the
   transcript in the store). Without them blame still works and says
   nothing about what was asked.
2. **Transport** (`save`, `list`, `restore`): `list_sessions` with the
   live-cwd/branch/version choice, `transcript_cwds`, `restore_path`,
   `existing_transcript` and `resume_command`. The rewrite itself is shared
   and needs nothing, as long as the transcript is text with paths in it.
3. **Conversation** (`ask`): `find_transcript` and `fork`, plus `title` if
   the harness keeps one outside the transcript.

**The degraded tier is a first-class case.** A harness with turn hooks and
no resumable transcript implements tier 1 alone, about 40 lines on Codex's
model. `save` finds none of its sessions, `restore` reports a bundled one
as `"status": "unsupported"`, and `ask` fails with `unsupported-agent`.
Blame and log are complete. A harness with no hooks at all gets no tier
today; the shape for it is a stop-only hook (each turn's end snapshot
becomes the next turn's start, which `stop` already handles) or, with no
signal at all, attribution to the span between two `snapshot` calls,
which nothing drives yet.

Codex took ~600 lines (`codex.rs`), most of it the format: where a turn
starts and ends, the live cwd, a prefix fork. The questions to answer
first for the next harness, in the order they decide the cost: does resume
need anything besides the transcript file (Codex: no; this is the one that
could make restore impractical); are hooks trusted or gated (Codex: gated);
what names a turn in the transcript; where the live cwd and branch are
recorded. Cursor, Gemini CLI and Copilot CLI have not been examined.
Nothing here is verified about them.

## Layout

One Rust crate at the root: a library holding every module, and the `loch`
binary on top of it. The toolchain is pinned in `rust-toolchain.toml`.

```
src/
├── main.rs       # argv handling, dispatch, the two renderers' entry point
├── cli.rs        # every command: list/save/restore/delete/reindex/ingest/install/
│                 #   blame/log/ask/backfill/push/pull/remote/gc/hook/help/version
├── agent.rs      # the adapter boundary: what loch needs from one agent harness
├── claude.rs     # Claude Code adapter: path encoding, discovery, metadata, turns, fork
├── codex.rs      # Codex adapter: rollout discovery, live cwd, turns by id, prefix fork
├── bundle.rs     # bundle format, gzip, content-addressed ref
├── format.rs     # loch format: turn and content ids, derived files
├── ingest.rs     # Claude Code JSONL -> loch turns (the converter)
├── index.rs      # store key layout, derived index entries, delete and reindex
├── snapshot.rs   # on-demand workspace snapshots with a stat cache, like git's index
├── attribution.rs # per-turn added lines, edit records, and blame's matcher
├── backfill.rs   # turns from before the hooks, from transcripts and commits
├── hook.rs       # `loch hook prompt|stop|end`: turn boundaries, closing, collection
├── trace.rs      # install, finding transcripts, describing turns, ask's fork
├── git.rs        # git by subprocess: a project's root commit, the current branch
├── blame_html.rs # `blame --html`: self-contained page (blame_template.html)
├── output.rs     # result envelope, the two renderers, structured failures
├── redact.rs     # secret scrubbing, applied on save before packing
├── rewrite.rs    # path rewriting (the load-bearing logic)
├── store.rs      # Store interface + local-fs backend, the local store
├── remote.rs     # named remotes in ~/.loch/config, and which one a command means
├── sync.rs       # push and pull: markers of what was sent, tombstones
├── s3.rs         # the S3-compatible backend, SigV4 by hand
├── json.rs       # JSON reading and writing, RFC 8785 canonical form
├── argparse.rs   # command-line parsing, usage errors and -h
├── py.rs         # string semantics: whitespace, lone surrogates, ordering
└── sys.rs        # paths, timestamps, OS error strings, hostname, uuid4
tests/            # integration tests, one file per module plus cli.rs
├── derived_reader/ # a verifying reader for derived files, shared by three tests
├── trace.rs      # hooks, blame, log and ask end to end, squash and clone included
├── codex.rs      # the Codex adapter end to end, on synthetic rollouts
├── backfill.rs   # backfill's two sources, contested commits, renames
└── corpus.rs     # opt-in, against this machine's transcripts (LOCH_CORPUS=1)
examples/bench_hook.rs # per-call wall clock of `loch hook`, any number of binaries
npm/              # @ness-dev/loch and its per-platform packages
```

`ingest.rs` normalizes with `rewrite.rs`'s machinery rather than its own:
`rewrite::compile` is the same single pass with the same guards, fed
placeholder targets (`{{loch:root}}`, `{{loch:home}}`, …) instead of another
machine's paths. That sharing is what makes a turn id survive `restore` — a
copy rewritten by one set of pairs normalizes back through the same set — so
a change to the guards changes both behaviours at once, which is the point.
`tests/corpus.rs` is the check that it still holds on real data: 3809 of 3810
sessions keep every turn id across a simulated move, and the one that doesn't
is a transcript that already contains the destination paths (SPEC.md §14.1).

The turn core holds the content's *hash*, not the content, so deleting
content never invalidates an id. Anything that makes a turn's id depend on
content directly breaks tombstones; anything that makes it depend on facts
(timestamps, record uuids, model, cwd) breaks stability across copies.

The store layout is:

```
bundles/<ref>.loch                  # the artifact
index/<dimension>/<value>/<ref>     # derived entry, small JSON payload
```

`branch` is the only dimension implemented, but it is a *dimension*, not a
special case: `index/cwd/...` can be added later by passing a different
name, with no migration and no format change. Don't collapse the
dimension segment back into the path.

Values are percent-encoded (`feature/foo` → `feature%2Ffoo`) so a segment
never contains `/`. **Do not reuse Claude's `[^a-zA-Z0-9] -> -` scheme
here** — it is lossy, and it would file `feature/foo` and `feature-foo`
under the same segment. A session with no branch is indexed under `%00`,
which no real value can encode to since a ref name can't hold a control
character.

Branch is a property of each *session*, not of the bundle: a `save`
without `--branch` can pack sessions from several branches, so one bundle
gets one entry per branch it touches. An entry carries only display
metadata — session count, cwd, timestamp, packed size, Claude version —
enough to list a branch without fetching a single bundle.

Ordering matters in both directions and for the same reason. `save`
writes the bundle before its entries, so a failure leaves an
unindexed bundle that `reindex` can recover. `delete` removes the entries
before the bundle, since the entries are derived from it and losing the
bundle first would strand them. `Store.delete` on an absent key succeeds,
which is what lets either repair run.

`claude.rs` decides **which** cwd a session gets, and everything in
`rewrite.rs` is downstream of that answer: `SessionMeta.cwd` becomes the
bundle manifest's cwd and then `RewriteSpec.origin_cwd`, so one wrong
value poisons the whole rewrite. Because a session can hold several cwds
(see above), it is chosen by comparing `encode_path` of each against the
name of the directory the transcript is filed under. **The encoding is
only ever compared here, never decoded** — it is lossy, so `<repo>/sub`
and `<repo>-sub` produce the same slug, which makes a match strong
evidence and a mismatch conclusive. That asymmetry is the whole safety
argument. Nothing matching — a hand-copied transcript, a renamed
directory — falls back to the first cwd rather than dropping the session.

`gitBranch` is stale in exactly the same way and decides which index entry
a bundle is filed under, so branch and version are read from the records
carrying the *chosen* cwd rather than from whichever record came first. A
branch taken from a directory the session has left is the same defect
wearing a different hat: the real session was reported on `main` when it
had spent 726 of its records on `feat/send-worktree-to-remote`, which
filed it under a branch nobody would look for it on.

The scan for cwd is deliberately **unbounded** where the rest of the
metadata scan used to be capped at 200 lines. The cap was the bug: the
live cwd first appeared at record 300, so no window wide enough to be
correct is a window at all. The cwds a session left are kept as
`SessionMeta.other_cwds` and surfaced as `otherCwds`, since a session
spanning two directories is something to know before shipping it, not
after.

**That is a real cost and it was paid knowingly.** `list` now parses
every line of every session in a project directory. On the worst
directory on one machine — 5197 sessions, 133MB — it went from ~5.8s to
~6.5s best-of-three, with noisier runs nearer 8s; 10–45% depending on
cache state.

Three measurements say don't buy it back with a window. **The scan depth
is not where it went**: parsing all 5197 sessions whole takes 0.42s,
against 0.76s for a 200-line window of each — the window is *slower*,
because those sessions average 12 lines, it never binds, and slicing the
line list costs more than it saves. **The obvious optimisation does
nothing**: skipping `json.loads` on lines with no `"cwd"` substring
measured 0.43s against 0.42s, a wash, since 58% of lines carry one and
the search is paid on all of them. So the regression is per-file overhead
— stat, read, summary redaction — and the residue of building three dicts
where the old loop broke out after one line. **And the directory is a
pathological outlier**: the next largest holds 281 sessions.

Optimise the per-file path if this ever matters. Reinstating a cap would
buy back a fraction of a second on one anomalous directory in exchange
for being silently wrong again.

`rewrite.rs` is where correctness actually lives. It does a **single
left-to-right pass** with longest-match-first alternation, so a
substitution's output can never be re-matched by a later pair. The hazard
it guards against: a cwd nested under a home directory getting
half-rewritten if the pairs were applied sequentially. Keep the
single-pass property if you touch it — one scan that tries every source at
each position, never a loop of `replace` calls. The scan is written by
hand (`Replacer::apply`) because the guards are lookaheads, which the
`regex` crate doesn't have; a source repeated with two targets maps to the
last one.

The second property is a **boundary rule**: a path is only substituted
where the match ends on a path-component boundary. Without it a sibling
directory that extends the origin's last component gets mangled —
`<repo>-worktrees/<branch>`, which is where Harness puts every worktree,
became the target path with its own tail duplicated. That failure was
**silent**, and that is the part worth remembering: the corrupted string
no longer contains the origin, so `residual_origin_paths` reported a clean
rewrite across 239 bad values in one restored session. The guards are
per-alternative zero-width lookaheads (`Guard`), which is what keeps them compatible
with the single pass — an alternative that fails its guard falls through
to a shorter one, so `/Users/mike/projector` still gets its home rewritten
after the cwd pair declines.

The asymmetry that sets the guards: **too strict fails loudly** (the path
survives and `residual_origin_paths` reports it), **too lenient fails
silently**. So the real-path guard rejects only what could continue a
filename (`[A-Za-z0-9._-]`) and allows every other follower, since a
transcript ends a path with `"`, `\`, `:`, `,`, a space or a newline far
more often than with `/`.

That "fails loudly" holds only for the failure it describes — a path that
*keeps* an origin marker. It does not hold in general, and assuming it did
cost a downstream consumer a restore that was 76% wrong while reporting
success. `residual_origin_paths` is a substring search for `origin_cwd`
and `origin_home`, so it is blind to any path that came out wrong *and*
well-formed. The way that happens: give the spec the wrong `origin_cwd`
and the records naming the right one decline the cwd pair (correctly — the
boundary guard is doing its job), fall through to the home pair, and
become a path on this machine that simply isn't there. The origin marker
is consumed on the way out, so the search comes back empty.

`foreign_cwds` is the check that isn't blind to it. It asks the rewritten
transcript what directory its records think they are in and reports
anything that isn't `target_cwd`, with counts — after a rewrite that
worked, every record names the target, so a clean result is real evidence
rather than the absence of one marker. It reads the top-level `cwd` field
per record, not a substring: `"cwd"` occurs inside tool output too, and a
detector that cries wolf gets ignored. **This is the field to gate on**;
`residual_origin_paths` remains necessary and is nowhere near sufficient.

Non-empty is not always a bug. A session that genuinely moved between
directories has records that no single target cwd can satisfy, since only
one directory is being restored into. It always means the restored
transcript is partly wrong about the filesystem, which is the thing a
caller needs to know, so it is reported rather than papered over: those
records keep whatever the home pair made of them (a plausible sibling on
the destination) and are counted in `foreignCwds`.

It reads the transcript **before** rewriting, and that is the whole
reason the entry is keyed on the origin path. A caller's next move is to
state a mapping for the directory that came out wrong, and the origin
path is that mapping's left-hand side — but it survives losslessly only
on the input. Recovering it from the restored value means assuming a
leading target home was rewritten from the origin's rather than having
been there all along: right most of the time, silently wrong otherwise,
undetectable when wrong. Handing a caller a guess to fix a guessing bug
would reproduce the defect this whole area exists to remove. `restored`
comes from putting each cwd through the same spec, so both facts are
given and neither is inferred.

**Leaving foreign cwds unrewritten is possible — it is just not better.**
The obvious objection, that it needs knowing which subtrees exist on the
destination, does not apply: loch has the exact set of candidate cwds
already, having computed them to choose one, and those specific strings
are known to be session working directories rather than dotfile or config
references. So the rule "the home pair declines a value exactly equal to
a known non-chosen session cwd" is an exact match against a small known
set, needs nothing from the far side, and fits the single pass — an
identity pair sorts ahead of the shorter home pair by length, matches,
and consumes the string, so the home pair never sees it.
`rewrite::compile` filters identity pairs out via `source != dest` and
would have to admit them deliberately, which is a small change rather
than a fight with the design. It is not built because reporting is
sufficient and `--map` supersedes it: a mapping makes those records
*correct*, where declining the rewrite only makes them loudly wrong.
Recorded because "it can't be done" would read later as a closed
question, and it isn't one.

**`--map <origin>=<target>`, not built, is the intended shape.** A
repeatable flag on `restore` adding path pairs to the `RewriteSpec`
alongside the cwd and home ones, so a session that moved between
directories can have *every* one of them land somewhere real instead of
one landing right and the rest being counted. It is the natural
successor to `foreignCwds`: the report tells a caller which directories
came out wrong, and the flag is how they say what those should become.
Nothing in `rewrite.rs` needs to change to accept it — pairs already
carry their own boundary guard and are already sorted longest-first, and
**that sort must keep being by length rather than by caller order**: the
single-pass guarantee depends on a nested path matching before the
shorter prefix that contains it, and caller order is arbitrary. This is
a "keep doing that" note, not a change.

What makes it safe is that **the caller states the mapping; loch never
infers it.** The first consumer (Harness "send worktree to remote") can
name all four paths without a guess: the origin worktree is what the
user selected, the origin repo root comes from
`git rev-parse --path-format=absolute --git-common-dir` stripped of its
`.git` tail, the destination worktree is what `addWorktree` returns, and
the destination repo root is a required argument on the caller because
it is the one value nothing on either side can derive. Note the
destination worktree must be `realpath`'d: git reports canonical paths,
so `/tmp/...` comes back as `/private/tmp/...` and a lookup keyed on the
uncanonical form silently misses. That is the same symlink resolution
`claude.rs` does, for the same reason.

The encoded slug takes a stricter rule for a reason that can't be fixed:
`encode_path` maps every non-alphanumeric to `-`, so `<repo>/sub` and
`<repo>-sub` encode to the *same string* and no lookahead can tell them
apart. It is therefore rewritten only as a complete component, which
declines `<encoded-repo>-worktrees-...` entirely. That is a known lossy
limitation, and the false negative is the deliberate half of it — don't
"fix" it by loosening the guard, that reintroduces the corruption. The
session-id pair is a UUID rather than a path and carries no guard.

`redact.rs` scrubs secrets out of a transcript on `save`, before packing.
It is **best-effort pattern matching and nothing more.** It finds
credentials with distinctive structure (AWS/GitHub/Slack/Stripe/
Anthropic/OpenAI/Google keys, JWTs, PEM blocks) plus one context rule for
`UPPERCASE_NAME=value`, which is the only way to catch an AWS secret
access key — 40 chars of base64 with no prefix. It will miss anything
shaped like ordinary text, and lowercase JSON keys such as
`"api_key": "..."` are deliberately out of scope because matching them
fires on prose. **A redacted bundle is not a safe bundle: rotate any
credential an agent has read.** The rules stay narrow on purpose — a
scrubber that mangles prose is one people disable. No entropy heuristics;
they fire on every hash, UUID, and base64 blob in a transcript.

Three properties it has to keep:

- **The local transcript is never modified.** `cli.rs` reads, redacts the
  in-memory copy, packs that. Redaction changes the packed bytes and
  therefore the ref; that is correct, the redacted bundle is the artifact.
- **Single pass, same discipline as `rewrite.rs`** — one left-to-right
  scan in which the next match is the earliest any rule makes, ties to rule
  order — so no rule can match inside another rule's replacement. How that
  scan is made fast without changing an answer is under Performance. The `env-assignment` value carries a `(?!\[REDACTED:)`
  lookahead for the same reason: without it, re-scanning
  `TOKEN=[REDACTED:env-assignment]` matches all over again.
- **Redacted output is re-scanned, and a surviving match aborts the
  save.** A rule that doesn't remove what it matched is a leak, so it
  fails loudly instead of uploading. Two related constraints: the
  replacement token holds no quote, backslash, or newline, so a JSONL
  line still parses; and the PEM rule (the only multi-line one) excludes
  `"` from its body so a BEGIN and an END in two different records can't
  merge into one match that swallows everything between them.

Counts are reported per session by rule name. **Never print the match**,
not even truncated — `save`'s stdout lands in the next agent's transcript.

`save` is not the only path transcript content escapes on, so `claude.rs`
scrubs the `list` summary too, in `display_summary`. That summary is the
first user message, it reaches a picker row and a pasted bug report without
a `save` in between, and it is the only such field. Two ordering points:
redact *before* truncating, since a cut can leave a secret's tail matching
no rule; and a summary that raises `RedactionError` becomes `[REDACTED]`
whole, because failing a listing over a label would be the wrong trade.

Measured against 350MB of real transcripts: 37 matches, all from
`env-assignment` and all of them source code naming a secret rather than
a literal secret (`HARNESS_TOKEN: process.env.X`,
`RPC_TOKEN_TTL = timedelta(...)`). Distinguishing a reference from a
value needs a parser, so that failure mode stays — it mangles a line of
code in the packed copy, it doesn't leak. The prefixed-key rules match
nothing in that corpus, which is the point of the word boundaries.

Two smaller things `bundle.rs` has to keep doing, because the ref is a
hash of the packed bytes and any drift changes a bundle's identity: JSON
is packed compactly (`separators=(",", ":")`, `ensure_ascii=False`), and
absent session metadata is omitted rather than serialized as `null`.
Packing is deterministic (`mtime=0`), so the same sessions always
produce the same ref.

## The JSON contract

`loch` is meant to be embedded in other tools — Harness is the first — and
a caller that scrapes prose breaks the moment someone rewords a sentence.
So every command takes `--json`, accepted either before or after the
subcommand, and answers with exactly one document on stdout:

```json
{"schema": 3, "ok": true, "command": "save", "ref": "...", "bytes": 331, ...}
```

`schema` is the envelope version. It exists so a consumer can detect a
format change it doesn't understand; bump it when a field changes meaning or
disappears, not when one is added. It went to 3 when `save` stopped writing to
`LOCH_STORE` (its `store` names this machine's store now) and `blame`, `log`,
`ask` and `backfill` lost `--store`.

**Built once per command, rendered twice.** Each `command_*` returns a
`Result` — the payload, the prose, and any stderr warnings — and `main()`
hands it to one renderer or the other. No command contains `if args.json`,
and that is the point: a payload assembled anywhere other than beside the
prose that describes it will rot the first time someone edits the prose.

**Failures are documents, on stdout.**

```json
{"schema": 3, "ok": false, "command": "restore", "code": "nothing-restored",
 "error": "nothing restored", "sessions": [{"status": "skipped", ...}]}
```

Stdout rather than stderr so a caller parses one stream unconditionally and
reads the verdict off the exit status and `ok`. Splitting the two across
streams would force it to guess which stream to read before it knows which
outcome it got. In JSON mode stderr stays empty, and the prose warnings
(`restore`'s skips) are carried as per-item status instead.

**`code` is the contract; `error` is for humans.** Reword a message freely,
never a code. The set today:

| code | meaning |
| --- | --- |
| `no-sessions` | `save` matched nothing |
| `redaction-failed` | a secret survived redaction; nothing was uploaded |
| `transcript-unreadable` | `save` found a transcript that isn't UTF-8; nothing was uploaded |
| `missing-ref` | `restore`/`delete` called without a ref |
| `bundle-not-found` | the store answered and the ref isn't there (for `restore`, not here and not on the remote) |
| `store-unreachable` | the store didn't answer |
| `bundle-unreadable` | the bytes arrived and won't unpack |
| `nothing-restored` | every session in the bundle was skipped |
| `author-key-unreadable` | `ingest --author-key` named a file that isn't an OpenSSH public key |
| `file-unreadable` | `blame`/`log`/`ask` named a file that can't be read |
| `not-linked` | `ask` on a line no recorded turn wrote |
| `transcript-missing` | a turn wrote the line but its transcript is in neither its agent's directory nor the store |
| `turn-not-found` | the transcript no longer holds the record a turn ended on |
| `unknown-agent` | `--agent` named an agent this build doesn't know |
| `unsupported-agent` | `ask` on a turn whose agent's adapter can't fork or resume |
| `session-not-found` | `delete --session` on a session with no record here (or on the remote) |
| `no-remote` | a command that needs a remote named none, and none is configured |
| `ambiguous-remote` | several remotes are configured, none is `origin`, and none was named |
| `unknown-remote` | no remote by that name |
| `remote-exists` | `remote add` of a name already taken |
| `invalid-uri` | a remote URI no store understands, or the local store itself |
| `config-unreadable` | `~/.loch/config` exists and won't parse (it is never overwritten) |
| `unknown-command` | no such subcommand |
| `usage` | argparse rejected the arguments (exit status 2) |
| `internal` | an unexpected exception |

The three store-facing codes — `bundle-not-found`, `store-unreachable`,
`bundle-unreadable` — are one failure to a human and three different
affordances to a caller: don't retry, retry, report corruption. Telling them
apart is why `StoreError` has `Missing` and `Unreachable` variants and an
unpack failure is its own error: a catch-all around a store call can only
produce the union. Map the store call *alone* to a store code, since
anything wider would relabel a bug in this process as a failure of the
store. (`s3-extra-missing` existed while S3 needed an optional Python
package; the binary carries its client, so it can't arise and is gone.)

**A payload never overwrites the envelope.** `schema`, `ok`, `command`,
`code` and `error` are the envelope's (`output::RESERVED`), and
`document` drops a payload key that reuses one. Until that rule existed,
`install`'s payload field `command`, the hooks' loch path, replaced the
envelope's, so `install --json` said `"command": "loch"` rather than
`"install"`. The field is `hookCommand` now. Nothing bumped `schema` for
it, since the envelope went back to its documented meaning, but a caller
that read the path from `install`'s `command` has to read `hookCommand`.

**Exactly one document per invocation, including parse failures.**
`argparse.rs` returns `-h` and a bad flag as `Exit` values rather than
printing and exiting, so `main()` renders both in the mode asked for. A bug
becomes `code: "internal"`, which in text mode is `loch: internal error: …`
on stderr.

**Raw values, not renderings.** `format_bytes` and the fixed-width columns
of `describe`/`describe_entry` belong to the text mode alone: the payload
carries the integer and the ISO-8601 `...Z` timestamp. An empty result is
`{"sessions": []}` with `ok: true`, never an error.

The `list --remote` payload is deliberately *not* `index.rs`'s stored entry
shape even though the fields line up. One is a storage format, the other is
a versioned wire contract, and they change for different reasons.

**A partly-wrong restore is reported, not failed.** `restore` carries
`foreignCwds` per session — `{"<path>": <count>}` of directories the
rewritten transcript still names other than the one it was restored into.
It is the field to gate on, because `residualOriginPaths` cannot see the
failure it exists for (above). It stays a warning rather than a `code`:
a session that genuinely moved between directories has records no target
cwd can satisfy, and refusing the restore would help nobody. The text
renderer prints both, and the prose warning is stderr-only as usual, so
JSON mode carries the same fact as a payload field and leaves stderr
empty. `save` and `list` carry the same signal ahead of time as
`otherCwds`.

**Several agents, additively.** Each session in `list`, `save` and
`restore` carries `agent` (`claude-code`, `codex`) and `agentVersion`.
`claudeVersion` keeps meaning Claude Code's version, so it is null on a
Codex session rather than reused. `list` and `save` cover every agent
unless `--agent` picks one, but **`install` writes Claude Code's hooks alone
unless asked** (`--agent codex`, or `--agent all`). Installing Codex's too
by default would put loch, and a review prompt, in every Codex session on the
machine, or with `--project` a `.codex/` into every Claude-only repo.
`install`'s top-level `scope`/`settings`/`added`/`replaced`/`removed` stay
the first agent's (Claude's by default), and `agents` lists each,
with `needsReview` true where the agent won't run the hooks just added
until a person approves them. A restore of a session whose agent this
build lacks is `"status": "unsupported"`. None of this changed a field's
meaning, so `schema` stays 2. Blame's payload gained nothing. Its `agent`
field now says `codex` for Codex turns, and `turn`/`conversation` are null
for them, since the loch converter reads Claude's format alone. Consumers
keying on `agent == "claude-code"` (the Ness blame gutter) should expect
the other value.

**One bundle, several agents, unchanged refs.** A bundle's `agent` is its
first session's, and a session of another agent carries its own `agent`
key. The key is omitted whenever it would equal the bundle's, so a bundle
of Claude's sessions, every Claude bundle ever made included, packs to the
bytes it always did (`tests/bundle.rs` pins them, and `tests/codex.rs`
checks a Claude-only `save` carries no new key).

**A bundle holding any other agent's session is `version: 2`.** v0.2.0 reads
`version` and nothing about agents. Given a Codex session at version 1, it
would write it into `~/.claude/projects` and report `"status": "restored"`,
`ok: true`, with a `claude --resume` line: the silent failure this file keeps
warning about. It refuses any version but 1, so at 2 it fails loudly as
`bundle-unreadable` ("unsupported bundle version 2"). Claude-only bundles
stay at 1 and keep their refs, and no ref of a version-2 bundle predates
the rule, so nothing moved. `Bundle::required_version` decides it, and both
cases are pinned. Index entries name their first session's agent.

**A bundle's session ids are untrusted input.** Both adapters build the
restored file's name from the id, and `resumeCommand` puts it in a shell
line, so an id of `../../x` or `/x` chose where `restore` wrote and `;` what
the command ran. `agent::is_safe_session_id` (non-empty, `[A-Za-z0-9._-]`,
no leading `.`, at most 128 bytes) is checked when a bundle is unpacked, so a
bundle failing it is `bundle-unreadable` as a whole, and by both
`find_transcript`s, whose ids come from edit records in the same shared
store. Real ids are UUIDs. The bad id is never echoed back.

**Never the matched text.** Redaction is reported as `{"<rule>": <count>}`
plus a `redacted` total, exactly as much as the prose summary carries and no
more — `save`'s stdout lands in the next agent's transcript. The one field
holding transcript content is `list`'s `summary` (the first 120 characters of
the first user message, which the text output already prints), and it is
scrubbed in `claude.rs` before it ever becomes a `SessionMeta`. It should
stay the only such field.

## Packaging and release

loch is one static binary, built for six targets: macOS arm64 and x64, Linux
arm64 and x64 (musl, statically linked, so one build runs on any
distribution, glibc or not) and Windows arm64 and x64. Its only runtime
dependency is `git` on PATH, for the commands that touch a repository.
zlib and rustls are compiled in.

**Releasing** is `./release.sh <version>`, which bumps `Cargo.toml`, runs
the checks, commits, tags and pushes. It refuses a dirty tree, a branch
behind origin, or an existing tag, because a bad tag is expensive to undo: a
tag-triggered run uses the workflow file *as of the tagged commit*, so a
broken workflow can't be repaired by re-running the tag. The tag has to be
deleted from the remote and re-cut.

`release.yml` calls `test.yml` first, so an untested tag can't publish. It
then refuses a tag that disagrees with `Cargo.toml`, builds the matrix,
checks the Linux binaries really are static, and attaches a tarball per
target, the npm packages and `SHA256SUMS` to the GitHub release. Run by hand
(`workflow_dispatch`), it does all of that except publishing, which is how
to rehearse. Actions must be pinned to versions that exist: `setup-uv`
stopped publishing floating major tags after `v7`, and `@v10` failed the
first release attempt.

**npm, the way Claude Code is shipped:** `@ness-dev/loch` with
`optionalDependencies` on `@ness-dev/loch-{darwin,linux,win32}-{arm64,x64}`,
each declaring `os`/`cpu` and holding one binary, so npm installs only the
host's. `npm/platforms.json` maps target triples to packages, and
`npm/assemble.mjs` lays out a release's packages from built binaries. The
release workflow builds the tarballs but **doesn't publish to npm yet**:
that needs the `@ness-dev` org and a token. Assembling, `npm pack` and a
global install were dry-run locally for darwin-arm64.

Linux packages aren't split by libc, which differs from Claude Code's.
Ness's resolver adds `-musl` on musl hosts, so it must not do that for loch.

**Embedders spawn the platform binary, never a Node shim.** Measured: going
through the wrapper's Node launcher costs ~24ms a call (27ms against 3ms),
which hands most of the hooks' speed back on every Bash call.
- **The path:** `index.js` exports `binaryPath()`, the same `createRequire`
  plus `app.asar` → `app.asar.unpacked` resolution Ness already uses for
  Claude Code.
- **Electron:** an app must `asarUnpack` `**/node_modules/@ness-dev/loch-*/**`.
- **Global installs:** for `npm install -g`, the postinstall replaces
  `bin/loch` with a hardlink to the native binary, so the `loch` on PATH is
  the binary itself. With `--ignore-scripts`, or on Windows, where npm runs
  bins through a `.cmd` wrapper anyway, the shim remains and still works.
- **Hooks:** Ness should keep writing the binary's absolute path with `loch
  install --local --cwd <worktree> --command <path>` (settings.local.json,
  since the path is machine-specific) rather than rely on PATH. It means
  "this worktree only", installs Claude Code's hooks alone, and writes the
  same lines it always did. A user who also ran a plain `loch install` is
  recorded once, since each event is handled once (`hook::claim`).
  Codex's need `--agent codex` (or `all`). With `--local` that is the
  worktree's `.codex/hooks.json`, which git sees and Codex runs only in a
  trusted project. Without it, it is the user's `$CODEX_HOME/hooks.json`,
  beside Ness's own status hooks. Either way, Ness should only ask with the
  user's say-so and must never rewrite that file wholesale. Codex won't run
  the hooks until they're trusted in `/hooks`.

**Builds reproduce.** `SOURCE_DATE_EPOCH` fixes `save`'s `createdAt`, the
one clock in a bundle's bytes, so the same sessions pack to the same ref.

**`uv tool install` is gone.** loch used to ship as a pure-Python wheel; a
machine that installed it that way has a `loch` on PATH that predates this
binary. It should `uv tool uninstall loch` and take a release tarball or the
npm package instead. Hook lines written by `install` name plain `loch`, so
PATH order decides which one the hooks run until the old one is removed.

## Why some of the code looks the way it does

loch was written in Python first and ported to Rust. Before the Python was
deleted, the port was held to it **byte for byte**: stdout, stderr, exit
status and every file written, over 51 synthetic scenarios and every project
directory on one machine (472, all identical). That standard is why several
modules reimplement things a crate would normally provide. Nothing requires
that compatibility any more, so change them freely, but know what each was
protecting:

- **`json.rs` instead of serde_json.** serde refuses lone surrogates, and
  **five transcripts on one machine hold them** (a `\ud83d` cut from its pair
  by truncation). It also rounds integers wider than 64 bits and rejects
  `NaN`. Every turn id is a hash over `json::canonical`, so how a transcript
  parses decides every id.
- **Lone surrogates are mapped into U+10F800..U+10FFFF** (`py.rs`), since a
  Rust `String` can't hold one. That covers both JSON escapes and
  `surrogateescape`d undecodable bytes. They are mapped back wherever they
  are written, and they sort, escape and canonicalize as the surrogate. The
  cost: a transcript holding those private-use code points literally would
  be misread. The corpus check asserts none on this machine does.
- **Whitespace is `py::is_space`**, the 29 code points that include
  U+001C..U+001F, not Unicode White_Space. Redaction's `\s` uses the same
  set.
- **`argparse.rs`** reproduces Python's argparse: abbreviation,
  `--opt=value`, `--`, the A/O/- matching that decides what a `nargs="?"`
  positional takes, and the help formatter's wrapping at `COLUMNS`. Usage
  errors exit 2.
- **zlib's own deflate**, through flate2's zlib backend (memLevel 8, raw
  window 15), behind a fixed gzip header (OS byte 255, XFL 2). The ref is a
  hash of those bytes, and miniz_oxide compresses differently. Changing
  either changes the ref of every bundle.
- **Error strings** follow Python's conventions: `[Errno 2] No such file or
  directory: '/x'`, botocore's `An error occurred (Code) when calling the Op
  operation: Message`. They reach the `error` field, which consumers
  shouldn't parse anyway.

Three defects in the Python turned up during the port and were fixed in both
builds before it was deleted:
- **Refs that varied by platform.** From 3.11, Python let zlib write the gzip
  OS byte, so the same sessions got a different ref on macOS and Linux. That
  is why the header is fixed now.
- **One bad byte took down a listing.** An undecodable byte took down `list`
  for a whole directory, which is why metadata decodes with replacement and
  `save` refuses such a session as `transcript-unreadable`.
- **`save` wasn't reproducible**, which is why `SOURCE_DATE_EPOCH` exists.

Known gaps:
- **S3 credentials:** the chain reads env and the shared files only. SSO,
  `credential_process`, assumed roles and instance metadata are not read.
- **Timestamps:** a naive (offset-less) ISO timestamp is read as UTC.
  Transcripts always carry `Z`.
- **Windows** has never been built.

## Performance

**Hooks run at a turn's boundaries, not around every shell command.** The
git-notes design needed a hook around every Bash call (73ms each in Python,
then 3ms in Rust with a native HEAD reader that is now gone). Attribution
needs a snapshot at the turn's start and end: one walk of the workspace, a
`stat` per file, and a read only of files whose size or mtime changed. A
project's first snapshot reads everything once. Measured with
`examples/bench_hook.rs` (median of 20, one file changed per turn):

| files | `prompt` | `stop` |
| --- | --- | --- |
| 2,000 | 43ms | 50ms |
| 20,000 | 75ms | 134ms |

**Redaction is most of `save`'s cost.** A backtracking engine tries all ten
rules at every position, and `env-assignment` can begin at any capital: over
15 project directories that was 150s in Python and 264s in a first Rust cut
with fancy-regex. `redact.rs` gives the same answers with a restructured
search, 15s:
- **The nine prefixed rules** are plain `regex` crate searches, linear and
  with leftmost-first semantics equal to backtracking. Their one-character
  boundary checks are made by hand.
- **`env-assignment`** is tried, anchored, only at the start of an
  `[A-Z0-9_]` run that holds a keyword. Every way it could match from inside
  the run is also a way from the run's start, so that is the only candidate.
- **The next match** is the earliest position any rule matches at, ties going
  to rule order.

The full alternation stays in the unit tests as an oracle, fuzzed against
the fast path over 40k generated strings that exercise every rule. The test
asserts each rule matched, so it can't pass vacuously.

**Codex discovery reads every rollout, on every `list` and `save`.** Both
cover every agent by default, so a Claude-only listing pays for it too: about
50ms for 10MB of rollouts, linear in their total size, and the rollouts
never age out, so a year of Codex use is paid on every listing. If it starts
to matter, the cheap fix is a substring check for the target cwd before the
full parse. Only the last `turn_context`'s cwd decides a match, so a rollout
whose bytes never contain the cwd string can't match and can be skipped.
The catch is matching through a symlink: Codex stores `-C` unresolved, so
the check has to try the target's unresolved spellings too, or skip only
when no form of it occurs. `--agent claude-code` avoids the scan today.

**S3 is SigV4 by hand over `ureq`/rustls**, not the AWS SDK: four request
kinds don't justify an async runtime and several MB. The signer is pinned to
AWS's two published examples (a header-signed GET and a signed query
string). Listings send `encoding-type=url` and decode the keys, so a key
with any character survives the XML. A store that doesn't answer fails
after 20s. No hook ever waits on that, since hooks write only locally. The tests that need an endpoint are
`#[ignore]`d in `tests/store.rs` and `tests/index.rs`, and read it from
`LOCH_TEST_S3_ENDPOINT` (bucket `LOCH_TEST_S3_BUCKET`, default `sessions`):
`moto_server` or MinIO will do. moto's verifier rejects correct signatures on
keys holding a space or `+` and on prefixed listings when its IAM auth is on,
so a signing failure in exactly those cases against moto is moto's.

## State

Working and verified end to end: a session was created on a feature
branch, saved, its bundle rewritten to look like it came from a
different machine (foreign home and repo path), restored into a
different directory with the original deleted so there was no fallback,
and resumed — it recalled content from the original conversation.

The multi-cwd fix is verified against the real session it was found on:
`save` now packs it under the worktree it ended in and indexes it under
`feat/send-worktree-to-remote` (it was packing the main repo and indexing
`main`), a cross-machine restore rewrites all 726 worktree records to the
target, and the 194 records from before the move are reported as
`foreignCwds` with `residualOriginPaths` still empty — which is precisely
the pair of facts the old output could not distinguish from success. The
regression tests fail against the previous selection rule, checked by
reverting it.

The same staleness had a second victim that was not in the brief:
`gitBranch`. It was read from the first record too, so that session
reported `main` and was filed under `index/branch/main` while 726 of its
records were on `feat/send-worktree-to-remote`. Branch and version are
now read from records carrying the *chosen* cwd. This is worth its own
release note rather than folding into the cwd fix, because the blast
radius is different: a consumer that carries the branch itself sees a
healthy-looking transfer while the loch entry sits under a name nobody
would look for it on, and every already-saved bundle from a moved session
is filed wrong until re-saved or reindexed.

Those 194 records are the honest limit. They name a directory that really
is different, so they keep whatever the home pair made of them and are
counted rather than corrected. `--map` (above) is what would make them
correct rather than merely visible; nothing needs it yet.

`otherCwds` and `foreignCwds` are new payload fields, so no `schema` bump
— but the first consumer (Harness "send worktree to remote") was reading
`residualOriginPaths` as its success signal and needs telling that the
field it wants is `foreignCwds`. Its shape is
`{"<origin path>": {"count": N, "restored": "<path on this machine>"}}`,
keyed on the origin deliberately (above) — it was `{"<path>": <count>}`
briefly and was changed before anything consumed it.

The bundle format is pinned by `tests/bundle.rs`: the exact packed JSON
and ref of a two-session bundle (an absent branch and version, non-ASCII,
escapes), and that re-packing an unpacked bundle reproduces its bytes. A
failure there means every existing ref would stop reproducing, which needs
a decision, not a test update. The adapter extraction was also checked
against real data: `save` and `list` on three project directories (100
sessions) gave identical refs and output before and after. Note that
`SessionMeta.other_cwds` is deliberately *not* packed into the bundle:
the transcript is the source of truth for what directories it names, and
adding a field would change the ref of every bundle for a value the
restore side can recompute.

**The S3 backend has still never run against a real bucket.** It has run
against moto, which is more than it had, but mocks agree with your
assumptions by construction. A live run failed on `Access Denied` (the
IAM user lacked `s3:PutObject`); the region mismatch that precedes that
failure surfaces as "The bucket you are attempting to access must be
addressed using the specified endpoint," so set `LOCH_S3_REGION`
when the bucket's region differs from the AWS config default.

Listing and deleting need IAM actions the bucket policy may not grant
yet: `s3:ListBucket` is **bucket-level**, on `arn:aws:s3:::<bucket>`
rather than `arn:aws:s3:::<bucket>/*` where `s3:GetObject` and
`s3:PutObject` live, and `s3:DeleteObject` is object-level. A missing
`s3:ListBucket` makes `list --remote` and `reindex` fail while `save`
and `restore` keep working.

Redaction on `save` is in and on by default, with no opt-out flag — if
verbatim bundles turn out to be needed, add one deliberately. Its
recall is untested against a real leak: the corpus it was measured on
contained no live credential to catch.

Branch indexing, `delete`, and `reindex` are in, verified against the
file store and moto. The old flat `<ref>.loch` layout was dropped without
a migration: nothing had been stored in it beyond throwaway local
bundles, since every S3 save so far failed on `Access Denied`.

`--json` covers every command, including the empty results and every failure
path, and the suite pins the two invariants a consumer actually leans on:
stdout parses as a single document with nothing else in it, and a failing
command still produces a parseable document with a nonzero exit status. The
error taxonomy was reviewed by its first consumer, which is where the split
between `bundle-not-found` and `store-unreachable` came from, but nothing has
been wired up against it yet, so the field names have never survived contact
with a real integration.

**Neither workflow has ever run** — the first tag push is also the first
execution of the release path, including the six-target build matrix, and no
release exists yet.

**Blame, log and ask run on content attribution.** Verified end to end in
`tests/trace.rs`:
- **Shell edits count.** Lines changed by `sed` and a file written by a
  generator are attributed to the turn that ran them, with no edit tool
  involved.
- **History rewrites don't matter.** After a squash, a fresh clone with an
  empty HOME and only a copy of the store blames the same lines to the same
  turns, and finds the transcripts in the store.
- **A person's text isn't the agent's.** A line someone later rewrote is
  unattributed, a re-indented one keeps its turn, and a person's lone `}` is
  not credited to the agent.
- **Cut-short turns keep their edits.** A cancelled turn is closed by the
  next prompt, a crash followed by `--resume` likewise, and quitting mid-turn
  by `SessionEnd`.
- **Collection is safe.** It removes earlier turns' snapshots and a killed
  write's leftovers, while an open turn's start survives until it's
  abandoned.

The notes written under the old design, on this repository only, are not
migrated: they sit on commits `main` no longer contains.

Known gaps:
- **A crash nobody resumes** loses its turn's attribution: nothing ever says
  when that turn ended. Its state expires after 7 days.
- **Sessions running at once in one workspace** each record the other's
  changes as their own.
- **The transcript snapshot** is one mutable key per session rather than
  content-addressed `raw/<hex>`, and subagent sidecars aren't snapshotted.

The rename from lochy carried no compatibility shim: `LOCHY_STORE`,
`LOCHY_S3_ENDPOINT`, `LOCHY_S3_REGION` and the `~/.lochy/store` default are
simply no longer read. That was chosen because nothing had been released and
no consumer set the old variables — but the failure it leaves is the silent
kind, so it is worth knowing about: a stale `LOCHY_STORE=s3://...` makes
`save` succeed into the local default with `ok: true`, and an unmoved
`~/.lochy` makes `list --remote` an empty, healthy-looking answer. If a
shim is ever wanted, the shape is to read `LOCH_*` first, fall back to the
old name, and warn on stderr in text mode only.

**Codex is verified end to end on real sessions** (codex-cli 0.166.0,
scratch repositories, a temporary store):
- **Hooks and blame.** `install --agent codex` wrote `.codex/hooks.json`,
  and a session of two `exec` turns, each editing `greet.py` through the
  shell, was recorded turn by turn under `edits/codex/`. `blame` gave
  lines 1–2 to the first turn and 3–6 to the second, with `"agent":
  "codex"` and Codex's own thread title. A human-committed line stayed
  unattributed.
- **A cancelled turn.** In the TUI, Esc mid-turn ran no `Stop`. The next
  prompt closed the turn as `interrupted`, keeping its edit, and the next
  turn recorded none of it.
- **ask.** `ask greet.py:1 --at-turn` forked at the first turn's
  `task_complete`, and resumed, the fork knew that turn's codeword and not
  the second's. Without `--at-turn` it knew both.
- **save and restore across machines.** The bundle was rewritten to
  another machine's home and repo path, then restored with `--new-id` into
  a different directory. It came back with `residualOriginPaths` and
  `foreignCwds` both empty, no trace of the foreign home or the old id in
  the file, every cwd naming the target, and `codex resume` recalled both
  codewords and named the new directory as its cwd.
- **Discovery.** All 70 local rollouts (four CLI versions) are found under
  their live cwd. A Claude `save` of 100 real sessions gave the same refs
  before and after the adapter extraction.
- **Install beside Ness.** A user-wide install against a copy of this
  machine's `~/.codex/hooks.json` kept every Ness group byte for byte and
  in place, and appended loch's.

- **User-wide install, real runs.** In a scratch `HOME`/`CODEX_HOME`, a
  user-wide install appended loch's groups after a Ness-style status hook,
  and `--local` appended them after a team hook in the repo's
  `.codex/hooks.json`. All of it was trusted through the TUI's "Trust all and
  continue", and the runs used no bypass flag. Each case below was one
  `exec` turn:
  - **The reviewer's lost-turn case.** Trust was removed from loch's project
    entries and kept on the team hook's. Codex ran the team hook and skipped
    loch's project one, and the user-wide hook recorded the turn. The
    inference this replaced would have deferred and lost it.
  - **Both trusted.** Both loch hooks ran, and the turn was recorded once,
    not interrupted, and blamed correctly.
  - **A plain directory.** The hooks ran (the status hook fired), and loch
    wrote nothing, not even a claim.

  Measured: a user-wide hook outside a repository takes 2.8ms (process
  start). The same 2,000-file directory without the gate takes 83ms, and
  gets snapshotted.

The TUI was driven by a script over a pty, so how the prompt and
`needsReview` read to a person is still untested.

Codex gaps: `ingest` and `backfill` are Claude-only (SPEC.md §8's converter;
see the tool-call note above); a moved session has no branch; a
`--local` Codex install is a file git sees, since Codex has no other; a
fork has no title; `archived_sessions` isn't searched.

Also missing: `restore --map` (design intent recorded above, deliberately
not built on this branch), any other index dimension (`cwd` is the
obvious next one, and the layout takes it without a migration), PR-level
git pointers for bundles, and no adapters beyond Claude Code and Codex.
`restore` still needs a full 64-character ref — the index makes refs
discoverable, but nothing resolves a prefix.

**The loch format (SPEC.md) is v1 draft.** `loch ingest` converts every
session in the local corpus (3810 sessions, 266k turns) and the derived files
read back verified and byte-identical; re-ingest is idempotent. The draft
questions are all settled in SPEC.md §13, including no salt on content
hashes: privacy of withheld content is not a goal. What it doesn't do yet: write anything to a store (derived
files go to `~/.loch/derived`, raw sources aren't snapshotted), convert
subagent sidecars, give each of a moved session's cwds its own root, capture
git pins, or emit back to Claude Code JSONL. Until the first conversation is
shared, ids may change between commits (SPEC.md §12). The bundle format and
everything `save`/`restore` do are unchanged.

`json::canonical` is RFC 8785: keys by UTF-16 code unit, numbers as
ECMAScript prints them, strings as JSON.stringify escapes them, integers
exact at any size.

## Conventions

- Verify before committing: `cargo fmt --check`,
  `cargo clippy --release --all-targets -- -D warnings` and
  `cargo test --release`. A change to how transcripts are read adds the
  corpus check (`LOCH_CORPUS=1 cargo test --release --test corpus -- --ignored`).
- Commit coherent changes as you go rather than batching.
- **Every command must be fully drivable by a program that never reads the
  human text.** A new command isn't finished until `--json` covers it —
  its result, its empty case, and its failures with a stable `code`. Build
  the payload where the prose is built, never in an `if args.json` beside a
  write call.
- Don't add comments that restate the code. The comments that exist mark
  non-obvious constraints (the single-pass rewrite, the lazy SDK import,
  symlink resolution) — preserve or update those rather than deleting.
- Don't write planning or design documents. Update this file instead.
- Treat transcripts as sensitive: they contain everything an agent read.
