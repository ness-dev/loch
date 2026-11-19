# loch

Save and restore agent coding sessions across machines.

Agent CLIs keep their transcripts in machine-local directories keyed by the
working directory they ran in. That makes a session effectively stuck to the
laptop that produced it — you can't hand a teammate the conversation that
produced a branch, and you can't pick one up on another machine.

`loch` packs those transcripts into a portable, content-addressed
bundle, stores it anywhere S3-compatible, and unpacks it on another machine
with the paths rewritten so the session actually resumes.

Currently supports Claude Code.

## Install

loch is a single static binary. Take the one for your platform from
[GitHub releases](https://github.com/ness-dev/loch/releases), for example:

```sh
curl -L https://github.com/ness-dev/loch/releases/download/v0.1.0/loch-0.1.0-aarch64-apple-darwin.tar.gz \
  | tar xz --strip-components 1 -C ~/.local/bin loch-0.1.0-aarch64-apple-darwin/loch
loch --version
```

Builds exist for macOS (`aarch64-apple-darwin`, `x86_64-apple-darwin`), Linux
(`aarch64-unknown-linux-musl`, `x86_64-unknown-linux-musl`, static, so any
distribution) and Windows (`aarch64-pc-windows-msvc`, `x86_64-pc-windows-msvc`).
S3 support is built in. The only runtime requirement is `git`, for the
commands that work on a repository.

From source, with a Rust toolchain:

```sh
cargo install --git https://github.com/ness-dev/loch --locked
```

**If you installed the old Python version** with `uv tool install`, remove it
first (`uv tool uninstall loch`), or whichever `loch` comes first on PATH is
the one your hooks run.

For development:

```sh
cargo build --release      # target/release/loch
cargo test --release
```

## Use

```sh
# what's on this machine for the current repo?
loch list
loch list --branch feature/checkout-flow

# pack them up and push to a store
loch save --branch feature/checkout-flow --store s3://my-bucket/sessions
# -> packed e71cdcb1... [feature/checkout-flow] — redacted 2 secrets
# -> ref 10482276de745032...

# on another machine: what's been saved for the branch I'm on?
loch list --remote --store s3://my-bucket/sessions
loch list --remote --branch feature/checkout-flow
loch list --all            # every branch in the store

loch restore 10482276de745032... --store s3://my-bucket/sessions
# -> cd /path/to/repo && claude --resume e71cdcb1-7c2c-410b-8c73-91cdf0cba4b8

# a transcript that shouldn't be there any more
loch delete 10482276de745032...

# rebuild the index from the bundles, if a save was interrupted
loch reindex
```

`save` bundles every session matching the filter, so a branch with several
sessions produces a single ref.

## Blame a line back to its conversation

```sh
loch install          # once per project: Claude Code hooks, committed in .claude/

loch blame src/format.rs
#     102-112  2026-11-18  session 37d467e5 "loch-format-v1" — asked: "none of those seem like open questions…"
#     113-120  not written in a recorded turn
loch blame --html src/format.rs   # the whole file in a browser, tinted by conversation

loch log src/format.rs:105        # the turns that wrote what line 105 says now
loch log src/format.rs:90-120     # ...or a range
loch log src/format.rs            # ...or every turn that ever changed the file

loch ask src/format.rs:105 "why not just json.dumps?"
# forks the conversation that wrote line 105 and resumes it in Claude Code,
# from where it ended; --at-turn stops it at the end of the turn that wrote it
```

`blame --html` writes the same thing as a page and opens it: every line of the
file tinted by the conversation that wrote it, a key of what each one asked,
and a line's `loch ask` command a click away. `--lines 1-20,90-120` limits it
to an excerpt.

**How it knows.** The hooks snapshot the project's files when you send a
prompt and again when the agent stops, and record which lines the turn added —
whether it used an edit tool, `sed`, a code generator or a formatter, since all
of them land on disk the same way. Those records go to the store as hashes of
the lines, not the code, and blame finds a line's turn by matching the text in
front of it. Nothing is written to git, so a squash merge, a rebase or a
cherry-pick loses nothing, and a teammate pointed at the same store sees the
same answers in their own clone. A line someone later rewrites by hand is, by
design, no longer the agent's. A turn you cancel, or one cut short by a crash,
is closed by your next prompt or by the session ending, so its edits are still
its own. Each session's transcript is copied into the store too — Claude Code
deletes its own after 30 days.

For history from before the hooks, `loch backfill` rebuilds what it can from
the transcripts' edit calls and the commits the agent made, and marks those
turns as backfilled.

Snapshots are only needed until a turn is recorded, so loch deletes old ones
itself every 50 snapshots, keeping about one compressed copy of each project;
`loch gc` does it on demand.

`ask` never touches the original session: it writes a new one holding the
conversation, so the follow-up sees today's code.

## Scripting it

Every command takes `--json`, before or after the subcommand, and prints one
document on stdout instead of the text above:

```sh
loch save --json --branch feature/checkout-flow --store /tmp/outbound
# {"schema":2,"ok":true,"command":"save","ref":"10482276...","bytes":4712,
#  "redacted":2,"sessions":[{"sessionId":"e71cdcb1-...","branch":"feature/checkout-flow",
#  "redactions":{"aws-access-key":1,"jwt":1}}],"indexed":[...]}

ref=$(loch save --json --store /tmp/outbound | jq -r .ref)
loch restore --json "$ref" --into /path/to/repo --store /tmp/outbound

loch --version --json
# {"schema":2,"ok":true,"command":"version","version":"0.1.0"}
```

Failures are documents too, on stdout, so one stream carries both outcomes:

```sh
loch restore --json deadbeef --store /tmp/outbound; echo "exit $?"
# {"schema":2,"ok":false,"command":"restore","code":"bundle-not-found",
#  "error":"could not read deadbeef from /tmp/outbound: ..."}
# exit 1
```

Branch on `code`, not on `error` — the codes are stable
(`no-sessions`, `redaction-failed`, `missing-ref`, `bundle-not-found`,
`store-unreachable`, `bundle-unreadable`, `transcript-unreadable`,
`nothing-restored`, `unknown-command`, `usage`, `internal`), the messages are
prose and will get reworded. A ref that isn't there is `bundle-not-found`, a
store that didn't answer is `store-unreachable`, and bytes that arrived but
won't unpack are `bundle-unreadable` — so a caller knows which is worth
retrying.
`schema` versions the envelope.
An empty result is `{"sessions": []}` with `ok: true`, not a failure, and
`restore` reports each session as `restored` or `skipped` with its own
`residualOriginPaths` rather than warning in prose.

## Layout of a store

```
bundles/<ref>.loch                 the bundle, named by the hash of its bytes
index/branch/<branch>/<ref>        a small entry per branch the bundle touches
transcripts/claude-code/<project>/<session>.jsonl
                                   latest snapshot of a session, from the stop hook
```

`loch list --remote` reads only the index, so listing a branch never
downloads a bundle. Entries are derived from the bundles and hold nothing
that isn't already in them — `loch reindex` rebuilds the whole index by
scanning `bundles/`, and drops entries no bundle backs.

Branch names are percent-encoded, so `feature/foo` stays one path segment
and doesn't collide with `feature-foo`. Every index write goes to its own
key, so two machines saving at the same time can't overwrite each other's
entries.

An S3 store needs `s3:GetObject` and `s3:PutObject` on
`arn:aws:s3:::<bucket>/*`, plus `s3:ListBucket` on `arn:aws:s3:::<bucket>`
itself for `list`/`reindex` and `s3:DeleteObject` for `delete`.

## Stores

| URI | Backend |
| --- | --- |
| `/some/path` or `file:///some/path` | local directory |
| `s3://bucket/prefix` | any S3-compatible endpoint, with the `s3` extra |

| Variable | Purpose |
| --- | --- |
| `LOCH_STORE` | default store URI (else `~/.loch/store`) |
| `LOCH_S3_ENDPOINT` | custom endpoint for R2, MinIO, Backblaze, ... |
| `LOCH_S3_REGION` | region override |

These were `LOCHY_*` before the rename, and the default store was
`~/.lochy/store`. The old names are not read, so an old variable is silently
ignored rather than rejected — rename it, and `mv ~/.lochy ~/.loch` to keep
existing bundles visible.

`loch ingest` converts sessions into the loch conversation format (see
[SPEC.md](SPEC.md)) and writes one derived file per session to
`~/.loch/derived`, or `--out <dir>`.

Credentials come from the standard AWS chain, so `AWS_ACCESS_KEY_ID` /
`AWS_SECRET_ACCESS_KEY`, a shared profile, or an instance role all work.

## How it works

Claude Code stores a session at
`~/.claude/projects/<cwd with every non-alphanumeric character replaced by ->/<session-id>.jsonl`,
using the symlink-resolved cwd (so `/tmp` is really `/private/tmp` on macOS).

A bundle is gzipped JSON holding each transcript verbatim plus a manifest
recording the origin machine's home directory, platform, and the cwd each
session ran in. The ref is the SHA-256 of the packed bytes.

On restore, the origin's paths are rewritten to local ones — the repo path,
the home directory, and the encoded project-directory form — in a single
left-to-right pass, longest match first, so a cwd nested under a home
directory can't be half-rewritten. A path is only substituted where it ends
on a component boundary, so a sibling directory that merely extends the
origin's last name — `<repo>-worktrees/<branch>` next to `<repo>` — is left
alone rather than mangled. The result is written to the local project
directory for the target cwd.

`--new-id` mints a fresh session id, rewriting the `sessionId` field on every
line to match the new filename. Claude Code keys off both, and a file where
they disagree resumes into a hybrid transcript.

## Limitations

- **Claude Code only.** Codex and Cursor are the intended next adapters;
  Cursor keeps chat state in a SQLite workspace database rather than files,
  so it needs a different capture strategy.
- **Same-agent resume only.** A Claude transcript can't be resumed by Codex —
  the tool-call schemas differ. Cross-agent handoff would mean injecting a
  normalized transcript as context, which is a different feature.
- **Path rewriting is textual.** Absolute paths from the origin machine are
  substituted, but a session that referenced files outside the repo will
  still point at paths that don't exist locally. Paths under a *sibling* of
  the origin cwd are deliberately not rewritten either — nothing in the text
  says where they should land on this machine, so guessing would corrupt
  them. `restore` warns when origin paths survive the rewrite, which is the
  intended outcome for both cases.
- **Redaction is best-effort.** `save` scrubs credentials with distinctive
  structure — AWS, GitHub, Slack, Stripe, Anthropic, OpenAI and Google keys,
  JWTs, PEM blocks — plus `UPPERCASE_NAME=value` assignments, which is the
  only way to catch an AWS secret access key. It will miss anything shaped
  like ordinary text. `list`'s one-line summary goes through the same rules,
  since it prints the first user message. **A redacted bundle is not a safe
  bundle: rotate any credential an agent has read.**
- **Transcripts are sensitive regardless.** They hold verbatim tool output —
  file contents, command output, API responses. Anything an agent read is in
  the bundle. Treat a store as being as sensitive as the repo it came from,
  and prefer a bucket you can delete from over anything append-only.

## License

MIT
