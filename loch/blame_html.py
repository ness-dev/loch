"""`loch blame --html`: the annotated file as one self-contained page.

A terminal can't hold what makes blame readable — background tints that don't
fight the code, a key that stays put, clicking a line to see what was asked —
so this renders the same payload `--json` carries into HTML with no external
assets, which opens anywhere and can be pasted into a post as-is.
"""

import html
import json
from typing import Any

# Soft enough to tint a background under code, distinct enough to tell apart
# in the margin. Cycled when a file has more conversations than colours.
PALETTE = (
    "#4f8cc9",
    "#d08a3a",
    "#8e6cc2",
    "#4a9e6b",
    "#c75b7a",
    "#3a9ea5",
    "#a8913a",
    "#6b7fd1",
)
NONE_COLOUR = "#9aa0a6"


def _colour(mark: str, order: dict[str, int]) -> str:
    if mark not in order:
        return NONE_COLOUR
    return PALETTE[order[mark] % len(PALETTE)]


def render(
    file: str,
    conversations: list[dict[str, Any]],
    hunks: list[dict[str, Any]],
    lines: dict[int, str],
    visible: list[tuple[int, int]] | None = None,
) -> str:
    order = {entry["mark"]: index for index, entry in enumerate(conversations)}
    by_mark = {entry["mark"]: entry for entry in conversations}

    def shown(number: int) -> bool:
        return visible is None or any(a <= number <= b for a, b in visible)

    rows: list[str] = []
    previous_mark: str | None = None
    previous_number: int | None = None
    for hunk in hunks:
        mark = hunk["mark"]
        colour = _colour(mark, order)
        for number in range(hunk["start"], hunk["end"] + 1):
            if not shown(number):
                continue
            if previous_number is not None and number != previous_number + 1:
                rows.append('<tr class="gap"><td colspan="4">⋯</td></tr>')
                previous_mark = None
            label = ""
            if mark != previous_mark:
                entry = by_mark.get(mark)
                if entry is not None:
                    asked = entry.get("prompt") or "(no prompt recorded)"
                    label = (
                        f'<span class="label">{html.escape(mark)} · '
                        f"{html.escape(_short(asked, 70))}</span>"
                    )
                elif mark == "+":
                    label = '<span class="label">not committed yet</span>'
                else:
                    label = '<span class="label">no conversation recorded</span>'
            text = html.escape(lines.get(number, "")) or "&nbsp;"
            rows.append(
                f'<tr data-mark="{html.escape(mark)}" data-line="{number}" '
                f'style="--c:{colour}">'
                f'<td class="bar"></td><td class="num">{number}</td>'
                f'<td class="code">{text}</td><td class="note">{label}</td></tr>'
            )
            previous_mark, previous_number = mark, number

    key: list[str] = []
    for entry in conversations:
        colour = _colour(entry["mark"], order)
        asked = entry.get("prompt") or (
            "(transcript not available)"
            if not entry.get("available")
            else "(no prompt recorded)"
        )
        commits = ", ".join(commit[:7] for commit in entry["commits"])
        key.append(
            f'<li data-mark="{html.escape(entry["mark"])}" style="--c:{colour}">'
            f'<span class="chip">{html.escape(entry["mark"])}</span>'
            f'<span class="asked">“{html.escape(asked)}”</span>'
            f'<span class="meta">session {html.escape(entry["session"][:8])}'
            f" · {html.escape(commits)}</span></li>"
        )

    data = json.dumps(
        {"file": file, "conversations": conversations}, ensure_ascii=False
    ).replace("</", "<\\/")
    count = len(conversations)
    title = f"{file} — {count} conversation{'s' if count != 1 else ''}"
    return TEMPLATE.format(
        title=html.escape(title),
        file=html.escape(file),
        key="".join(key),
        rows="".join(rows),
        data=data,
    )


def _short(text: str, limit: int) -> str:
    flat = " ".join(text.split())
    return flat if len(flat) <= limit else flat[: limit - 1] + "…"


def parse_ranges(spec: str) -> list[tuple[int, int]]:
    """`1-12,99-110` → [(1, 12), (99, 110)]; a bare number is one line."""
    ranges = []
    for part in spec.split(","):
        part = part.strip()
        if not part:
            continue
        start, _, end = part.partition("-")
        first = int(start)
        ranges.append((first, int(end) if end else first))
    return ranges


TEMPLATE = """<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light dark">
<title>{title}</title>
<style>
.loch-blame {{
  --bg: #fbfbfa; --fg: #1f2328; --muted: #6a737d; --rule: #e6e6e3;
  font: 14px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
  color: var(--fg); background: var(--bg);
  display: grid; grid-template-columns: minmax(0, 1fr) 22rem; gap: 0;
  border: 1px solid var(--rule); border-radius: 8px; overflow: hidden;
}}
@media (prefers-color-scheme: dark) {{
  .loch-blame {{ --bg: #16181b; --fg: #e6e6e6; --muted: #9aa0a6; --rule: #2b2f34; }}
}}
.loch-blame header {{
  grid-column: 1 / -1; padding: 12px 16px; border-bottom: 1px solid var(--rule);
  font-weight: 600;
}}
.loch-blame header small {{ color: var(--muted); font-weight: 400; margin-left: 8px; }}
.loch-blame .file {{ overflow-x: auto; }}
.loch-blame table {{
  border-collapse: collapse; width: 100%;
  font: 12.5px/1.6 ui-monospace, SFMono-Regular, Menlo, monospace;
}}
.loch-blame tr {{ cursor: pointer; }}
.loch-blame tr[data-mark] td {{
  background: color-mix(in srgb, var(--c) 7%, transparent);
}}
.loch-blame tr[data-mark]:hover td {{
  background: color-mix(in srgb, var(--c) 16%, transparent);
}}
.loch-blame .bar {{ width: 4px; padding: 0; background: var(--c) !important; }}
.loch-blame .num {{
  width: 1%; padding: 0 10px; text-align: right; color: var(--muted);
  user-select: none; white-space: nowrap;
}}
.loch-blame .code {{ width: 100%; padding: 0 12px; white-space: pre; }}
.loch-blame .note {{ padding: 0 12px 0 0; }}
.loch-blame .label {{
  display: block; max-width: 15rem; overflow: hidden; text-overflow: ellipsis;
  font: 11.5px/1.9 -apple-system, BlinkMacSystemFont, sans-serif;
  color: color-mix(in srgb, var(--c) 75%, var(--fg));
  white-space: nowrap; text-align: right;
}}
.loch-blame tr.gap td {{
  color: var(--muted); text-align: center; padding: 2px; cursor: default;
  border-top: 1px dashed var(--rule); border-bottom: 1px dashed var(--rule);
}}
.loch-blame.focused tr[data-mark]:not(.on) {{ opacity: .28; }}
.loch-blame.focused tr[data-mark]:not(.on) .label {{ visibility: hidden; }}
.loch-blame aside {{
  border-left: 1px solid var(--rule); padding: 12px 14px;
  position: sticky; top: 0; align-self: start; max-height: 100vh; overflow: auto;
}}
.loch-blame aside h2 {{
  font-size: 11px; letter-spacing: .06em; text-transform: uppercase;
  color: var(--muted); margin: 4px 0 8px;
}}
.loch-blame ul {{ list-style: none; margin: 0 0 16px; padding: 0; }}
.loch-blame li {{
  display: grid; grid-template-columns: auto 1fr; gap: 2px 8px;
  padding: 8px; border-radius: 6px; cursor: pointer;
}}
.loch-blame li:hover, .loch-blame li.on {{
  background: color-mix(in srgb, var(--c) 14%, transparent);
}}
.loch-blame .chip {{
  grid-row: span 2; width: 22px; height: 22px; border-radius: 5px;
  background: var(--c); color: #fff; font-weight: 700; font-size: 12px;
  display: grid; place-items: center;
}}
.loch-blame .asked {{ font-size: 13px; }}
.loch-blame .meta {{ font-size: 11.5px; color: var(--muted); }}
.loch-blame .detail {{ font-size: 13px; color: var(--muted); }}
.loch-blame .detail code {{
  display: block; margin-top: 6px; padding: 8px; border-radius: 6px;
  background: color-mix(in srgb, var(--fg) 6%, transparent); color: var(--fg);
  font: 12px ui-monospace, Menlo, monospace; white-space: pre-wrap; word-break: break-all;
}}
.loch-blame .detail button {{
  margin-top: 6px; font: inherit; font-size: 12px; padding: 3px 10px;
  border-radius: 5px; border: 1px solid var(--rule); background: transparent;
  color: var(--fg); cursor: pointer;
}}
@media (max-width: 800px) {{
  .loch-blame {{ grid-template-columns: 1fr; }}
  .loch-blame aside {{ border-left: 0; border-top: 1px solid var(--rule); position: static; }}
}}
body {{ margin: 24px; background: Canvas; }}
</style>
</head>
<body>
<div class="loch-blame">
  <header>{file}<small>loch blame</small></header>
  <div class="file"><table>{rows}</table></div>
  <aside>
    <h2>Conversations</h2>
    <ul>{key}</ul>
    <h2>Line</h2>
    <div class="detail">Click a line to see the conversation that wrote it.</div>
  </aside>
</div>
<script>
(() => {{
  const data = {data};
  const root = document.querySelector(".loch-blame");
  const detail = root.querySelector(".detail");
  const byMark = Object.fromEntries(data.conversations.map(c => [c.mark, c]));
  const focus = mark => {{
    const same = root.classList.contains("focused") && root.dataset.focus === mark;
    root.classList.toggle("focused", !same && !!mark);
    root.dataset.focus = same ? "" : mark;
    root.querySelectorAll("[data-mark]").forEach(el =>
      el.classList.toggle("on", !same && el.dataset.mark === mark));
  }};
  root.querySelectorAll("li[data-mark]").forEach(li =>
    li.addEventListener("click", () => focus(li.dataset.mark)));
  const text = s => {{ const d = document.createElement("div"); d.textContent = s; return d.innerHTML; }};
  root.querySelectorAll("tr[data-line]").forEach(tr => tr.addEventListener("click", () => {{
    const c = byMark[tr.dataset.mark];
    const line = tr.dataset.line;
    if (!c) {{
      detail.innerHTML = `Line ${{line}}: ${{tr.dataset.mark === "+" ? "not committed yet" : "no conversation recorded"}}.`;
      return;
    }}
    const command = `loch ask ${{data.file}}:${{line}}`;
    detail.innerHTML =
      `<b>Line ${{line}}</b> was written in session ${{text(c.session.slice(0, 8))}}, asked:` +
      `<p>“${{text(c.prompt || "(no prompt recorded)")}}”</p>` +
      `Follow it up:<code>${{text(command)}}</code><button>Copy</button>`;
    detail.querySelector("button").addEventListener("click", e => {{
      navigator.clipboard.writeText(command);
      e.target.textContent = "Copied";
    }});
  }}));
}})();
</script>
</body>
</html>
"""
