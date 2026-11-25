#!/usr/bin/env python3
"""Deterministically generate Kipo SVG visual assets under docs/assets/.

Run `python3 scripts/render_readme_assets.py` after editing this file. Every
asset is self-contained: no external URLs, fonts, images, or scripts.
"""

from pathlib import Path
from xml.sax.saxutils import escape
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "docs" / "assets"

SANS = 'ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", "Inter", Helvetica, Arial, sans-serif'
MONO = 'ui-monospace, SFMono-Regular, "SF Mono", "JetBrains Mono", Menlo, Consolas, "Liberation Mono", monospace'

# Brand palette. The video trailer uses the same values.
INK = "#0A0E16"
INK_TOP = "#1A2335"
INK_RAISED = "#121A28"
INK_LINE = "#243049"
PAPER = "#F4F7FB"
MUTED = "#8C99AD"
LIME = "#B4F04A"
LEAF = "#7DD321"
AMBER = "#FFC940"
RED = "#FF5C5C"
CYAN = "#5CC8FF"

DARK = {
    "bg": INK,
    "card": INK_RAISED,
    "line": INK_LINE,
    "fg": PAPER,
    "muted": MUTED,
    "lime": LIME,
    "lime_text": LIME,
    "lime_soft": "#1C2A12",
    "cyan": CYAN,
    "cyan_soft": "#10263A",
    "amber": AMBER,
    "amber_soft": "#2E2510",
    "red": RED,
    "red_soft": "#33161A",
    "badge": "#18233A",
    "glow": LIME,
    "glow_opacity": "0.16",
}

LIGHT = {
    "bg": "#FBFCFD",
    "card": "#FFFFFF",
    "line": "#DCE2EA",
    "fg": "#0B1220",
    "muted": "#5A6678",
    "lime": LEAF,
    "lime_text": "#3B7D0C",
    "lime_soft": "#EEF9DD",
    "cyan": "#0B7BC0",
    "cyan_soft": "#E3F2FC",
    "amber": "#B7791F",
    "amber_soft": "#FDF3DC",
    "red": "#D93636",
    "red_soft": "#FDE8E8",
    "badge": "#F1F4F8",
    "glow": LEAF,
    "glow_opacity": "0.12",
}


def write_svg(path: Path, content: str) -> None:
    text = content.strip() + "\n"
    root = ET.fromstring(text)
    if not root.tag.endswith("svg"):
        raise ValueError(f"{path}: root element is not <svg>")
    if "viewBox" not in root.attrib:
        raise ValueError(f"{path}: missing viewBox attribute")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def t(s: str) -> str:
    return escape(s)


def style(extra: str = "") -> str:
    return (
        "<style>\n"
        f"    .sans {{ font-family: {SANS}; }}\n"
        f"    .mono {{ font-family: {MONO}; }}\n"
        f"{extra}"
        "  </style>"
    )


def mark(x: float, y: float, size: float, uid: str) -> str:
    """The Kipo mark: ink tile, white stem, split chevron, pollen dot. Drawn on a 256 grid."""
    s = size / 256
    return f"""<g transform="translate({x} {y}) scale({s:.5f})">
    <defs>
      <linearGradient id="tile-{uid}" x1="0" y1="0" x2="0" y2="1">
        <stop offset="0" stop-color="{INK_TOP}"/>
        <stop offset="1" stop-color="{INK}"/>
      </linearGradient>
    </defs>
    <rect width="256" height="256" rx="60" fill="url(#tile-{uid})"/>
    <rect x="1.5" y="1.5" width="253" height="253" rx="58.5" fill="none" stroke="#FFFFFF" stroke-opacity="0.09" stroke-width="3"/>
    <rect x="56" y="56" width="40" height="144" rx="9" fill="{PAPER}"/>
    <path d="M110 128 L160 56 L202 56 L152 128 Z" fill="{LIME}"/>
    <path d="M110 128 L152 128 L202 200 L160 200 Z" fill="{LEAF}"/>
    <circle cx="188" cy="128" r="13" fill="{AMBER}"/>
  </g>"""


def pill(x: float, y: float, label: str, fg: str, bg: str, stroke: str, size: float = 12, h: float = 24, char_w: float = 0.62) -> str:
    w = len(label) * size * char_w + 22
    return f"""<g transform="translate({x} {y})">
      <rect width="{w:.1f}" height="{h}" rx="{h / 2}" fill="{bg}" stroke="{stroke}"/>
      <text x="{w / 2:.1f}" y="{h / 2 + size * 0.36:.1f}" text-anchor="middle" class="mono" font-size="{size}" font-weight="700" fill="{fg}">{t(label)}</text>
    </g>"""


# Small 24x24 line icons, drawn around a 0..24 box.
def icon_recall(color: str) -> str:
    return f"""<path d="M4.5 12a7.5 7.5 0 1 0 2.2-5.3" fill="none" stroke="{color}" stroke-width="2.4" stroke-linecap="round"/>
      <path d="M3.5 3.8v4.6h4.6" fill="none" stroke="{color}" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/>
      <path d="M12 8v4.4l2.8 1.8" fill="none" stroke="{color}" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/>"""


def icon_shield(color: str) -> str:
    return f"""<path d="M12 2.5l8 3v6.2c0 4.9-3.4 8.6-8 9.8-4.6-1.2-8-4.9-8-9.8V5.5z" fill="none" stroke="{color}" stroke-width="2.4" stroke-linejoin="round"/>
      <path d="M9 9l6 6M15 9l-6 6" stroke="{color}" stroke-width="2.4" stroke-linecap="round"/>"""


def icon_moon(color: str) -> str:
    return f"""<path d="M19.5 14.5A8 8 0 0 1 9.5 4.5a8 8 0 1 0 10 10z" fill="none" stroke="{color}" stroke-width="2.4" stroke-linejoin="round"/>
      <path d="M17 3v4M15 5h4" stroke="{color}" stroke-width="2" stroke-linecap="round"/>"""


def hero_svg(dark: bool) -> str:
    c = DARK if dark else LIGHT
    w, h = 1200, 400
    cards = [
        ("recall", "Cross-session recall", "Yesterday's fix, back in today's prompt.", "≤ 250 tokens", c["cyan"], c["cyan_soft"], icon_recall),
        ("shield", "Live command shield", "Stops banned commands before they run.", "PreToolUse", c["lime_text"], c["lime_soft"], icon_shield),
        ("dream", "Overnight skill evolution", "Dead ends become SKILL.md while you sleep.", "kp dream", c["amber"], c["amber_soft"], icon_moon),
    ]
    card_w, gap, x0, y0, card_h = 354, 21, 48, 214, 140
    card_svg = []
    for i, (_, title, desc, chip, accent, soft, icon) in enumerate(cards):
        x = x0 + i * (card_w + gap)
        chip_w = len(chip) * 12 * 0.62 + 22
        card_svg.append(f"""<g transform="translate({x} {y0})">
    <rect width="{card_w}" height="{card_h}" rx="16" fill="{c['card']}" stroke="{c['line']}" stroke-width="1.5"/>
    <rect x="24" y="22" width="40" height="40" rx="11" fill="{soft}"/>
    <g transform="translate(32 30) scale(1)">{icon(accent)}</g>
    <rect x="{card_w - 24 - chip_w:.1f}" y="30" width="{chip_w:.1f}" height="24" rx="12" fill="none" stroke="{accent}" stroke-opacity="0.55"/>
    <text x="{card_w - 24 - chip_w / 2:.1f}" y="46.5" text-anchor="middle" class="mono" font-size="12" font-weight="700" fill="{accent}">{t(chip)}</text>
    <text x="24" y="94" class="sans" font-size="20" font-weight="750" fill="{c['fg']}" letter-spacing="-0.2">{t(title)}</text>
    <text x="24" y="120" class="sans" font-size="14.5" fill="{c['muted']}">{t(desc)}</text>
  </g>""")
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="100%">
  <defs>
    <radialGradient id="hero-glow" cx="0.86" cy="0.05" r="0.6">
      <stop offset="0" stop-color="{c['glow']}" stop-opacity="{c['glow_opacity']}"/>
      <stop offset="1" stop-color="{c['glow']}" stop-opacity="0"/>
    </radialGradient>
  </defs>
  {style()}
  <rect width="{w}" height="{h}" rx="20" fill="{c['bg']}"/>
  <rect width="{w}" height="{h}" rx="20" fill="url(#hero-glow)"/>
  <rect x="0.75" y="0.75" width="{w - 1.5}" height="{h - 1.5}" rx="19.25" fill="none" stroke="{c['line']}" stroke-width="1.5"/>

  {mark(48, 46, 104, 'hero')}
  <text x="176" y="104" class="sans" font-size="62" font-weight="800" letter-spacing="-2" fill="{c['fg']}">Kipo</text>
  <rect x="326" y="62" width="62" height="38" rx="10" fill="{c['badge']}" stroke="{c['line']}"/>
  <text x="357" y="88" text-anchor="middle" class="mono" font-size="20" font-weight="800" fill="{c['lime_text']}">kp</text>
  <text x="178" y="140" class="sans" font-size="21" font-weight="600" fill="{c['fg']}">Your coding agent remembers yesterday's fix and refuses the commands you banned.</text>
  <text x="178" y="170" class="mono" font-size="13.5" fill="{c['muted']}">Claude Code · Codex · Cursor · Windsurf  ·  one local kp.sqlite file  ·  no cloud account</text>

  {''.join(card_svg)}
</svg>"""


def logo_svg() -> str:
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" width="256" height="256">
  <title>Kipo</title>
  {mark(0, 0, 256, 'logo')}
</svg>"""


# Terminal demo -----------------------------------------------------------

TERM_W, TERM_H = 980, 478
CYCLE_S = 18
CHAR_W = 8.43  # 14px monospace advance
BOLD = ' font-weight="700"'


def terminal_demo_svg() -> str:
    # Each row: (step start %, kind, segments). kind "cmd" types itself in.
    # segments: list of (text, color var, bold).
    rows = [
        (0, "cmd", [("$ ", "--lime", True), ("kp connect claude-code --project . --path ~/.kp/personal --as alice", "--fg", False)]),
        (6, "out", [("✓ ", "--lime", True), ("hooks installed  ", "--fg", False), ("SessionStart · UserPromptSubmit · PreToolUse · Stop", "--muted", False)]),
        (None, "gap", []),
        (16, "out", [("# next morning: new session, empty context window", "--dim", False)]),
        (19, "cmd", [("claude › ", "--cyan", True), ("fix the flaky settle_invoice test", "--fg", False)]),
        (26, "out", [("kp ▸ ", "--cyan", True), ("recalled 2 memories in 186 tokens", "--cyan", False)]),
        (28, "out", [("  [dead-end]   ", "--amber", True), ("Retrying the HTTP mock never helped. The race is the lock order.", "--fg", False)]),
        (30, "out", [("  [constraint] ", "--amber", True), ("Database freeze until Monday 09:00 UTC. No schema migrations.", "--fg", False)]),
        (None, "gap", []),
        (42, "cmd", [("claude › ", "--cyan", True), ("Bash(\"sqlx migrate run\")", "--fg", False)]),
        (49, "out", [("✕ BLOCKED ", "--red", True), ("by PreToolUse  matches [forbid: sqlx migrate]", "--red", False)]),
        (51, "out", [("  why       ", "--muted", False), ("Database freeze until Monday 09:00 UTC  [kp-ref:a81f29c4]", "--fg", False)]),
        (53, "out", [("  override  ", "--muted", False), ("kp allow kp-ref:a81f29c4", "--lime", False), ("   single use, expires in 10 min", "--muted", False)]),
        (None, "gap", []),
        (66, "cmd", [("$ ", "--lime", True), ("kp forget release-freeze", "--fg", False)]),
        (72, "out", [("✓ ", "--lime", True), ("withdrew release-freeze  ", "--fg", False), ("1 dependent warning cleared in 0 ms", "--muted", False)]),
        (78, "prompt", [("$ ", "--lime", True)]),
    ]

    css_rules = []
    body = []
    y = 82
    for i, (start, kind, segs) in enumerate(rows):
        if kind == "gap":
            y += 14
            continue
        assert start is not None
        if start == 0:
            css_rules.append(f"    @keyframes r{i} {{ 0%, 95% {{ opacity: 1; }} 97.5%, 100% {{ opacity: 0; }} }}")
        else:
            css_rules.append(
                f"    @keyframes r{i} {{ 0%, {start - 0.01}% {{ opacity: 0; }} {start}%, 95% {{ opacity: 1; }} 97.5%, 100% {{ opacity: 0; }} }}"
            )
        css_rules.append(f"    .r{i} {{ animation: r{i} {CYCLE_S}s linear infinite; }}")
        spans = "".join(
            f'<tspan fill="var({color})"{BOLD if bold else ""}>{t(text)}</tspan>' for text, color, bold in segs
        )
        line = f'<text x="0" y="{y}" xml:space="preserve">{spans}</text>'
        extra = ""
        if kind == "cmd":
            prefix_w = len(segs[0][0]) * CHAR_W
            typed = len(segs[1][0])
            type_pct = min(5.5, 0.12 * typed)
            css_rules.append(
                f"    @keyframes c{i} {{ 0%, {start}% {{ transform: translateX(0px); animation-timing-function: steps({typed}, end); }} "
                f"{start + type_pct:.2f}%, 100% {{ transform: translateX({typed * CHAR_W + 12:.1f}px); }} }}"
            )
            css_rules.append(f"    .c{i} {{ animation: c{i} {CYCLE_S}s linear infinite; }}")
            extra = (
                f'<rect class="cover c{i}" x="{prefix_w:.1f}" y="{y - 16}" width="{typed * CHAR_W + 12:.1f}" height="22" fill="var(--bg)"/>'
            )
        if kind == "prompt":
            extra = f'<rect class="cursor" x="{2 * CHAR_W:.1f}" y="{y - 14}" width="9" height="18" rx="1" fill="var(--lime)"/>'
        body.append(f'<g class="r{i}">{line}{extra}</g>')
        y += 25

    css = "\n".join(css_rules)
    step_labels = [("1", "connect", 0), ("2", "recall", 16), ("3", "guard", 42), ("4", "forget", 66)]
    step_css = []
    step_svg = []
    sx = TERM_W - 28
    for idx, (num, label, start) in reversed(list(enumerate(step_labels))):
        end = step_labels[idx + 1][2] if idx + 1 < len(step_labels) else 95
        wpx = len(label) * 7.2 + 34
        sx -= wpx
        step_css.append(
            f"    @keyframes s{num} {{ 0%, {max(start - 0.01, 0)}% {{ opacity: 0.35; }} {start}%, {end - 0.01}% {{ opacity: 1; }} {end}%, 100% {{ opacity: 0.35; }} }}"
        )
        step_css.append(f"    .s{num} {{ animation: s{num} {CYCLE_S}s linear infinite; }}")
        step_svg.append(
            f'<g class="s{num}"><rect x="{sx:.1f}" y="12" width="{wpx - 6:.1f}" height="18" rx="9" fill="var(--chip)"/>'
            f'<text x="{sx + (wpx - 6) / 2:.1f}" y="25" text-anchor="middle" class="mono" font-size="11" fill="var(--fg)">{num} {label}</text></g>'
        )
    css += "\n" + "\n".join(step_css)

    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {TERM_W} {TERM_H}" width="100%">
  <style>
    :root {{
      --bg: #0B0F17;
      --chrome: #121826;
      --border: #263049;
      --chip: #1C2537;
      --fg: #E8EDF5;
      --muted: #8C99AD;
      --dim: #5D6A80;
      --lime: {LIME};
      --cyan: {CYAN};
      --amber: {AMBER};
      --red: {RED};
    }}
    .mono {{ font-family: {MONO}; }}
    .term {{ font-family: {MONO}; font-size: 14px; }}
    .title {{ font-family: {SANS}; font-size: 12.5px; font-weight: 600; }}
    .cursor {{ animation: blink 1.1s steps(2, end) infinite; }}
    @keyframes blink {{ 0%, 49% {{ opacity: 1; }} 50%, 100% {{ opacity: 0; }} }}
{css}
    @media (prefers-reduced-motion: reduce) {{
      .term g, .steps g {{ animation: none !important; opacity: 1 !important; }}
      .cover {{ display: none; }}
    }}
  </style>

  <rect width="{TERM_W}" height="{TERM_H}" rx="14" fill="var(--bg)"/>
  <path d="M14 0.75 H{TERM_W - 14} Q{TERM_W - 0.75} 0.75 {TERM_W - 0.75} 14 V42 H0.75 V14 Q0.75 0.75 14 0.75 Z" fill="var(--chrome)"/>
  <rect x="0.75" y="0.75" width="{TERM_W - 1.5}" height="{TERM_H - 1.5}" rx="13.25" fill="none" stroke="var(--border)" stroke-width="1.5"/>
  <line x1="0.75" y1="42" x2="{TERM_W - 0.75}" y2="42" stroke="var(--border)"/>
  <circle cx="24" cy="21" r="6" fill="#FF5F57"/>
  <circle cx="44" cy="21" r="6" fill="#FEBC2E"/>
  <circle cx="64" cy="21" r="6" fill="#28C840"/>
  {mark(88, 11, 20, 'term')}
  <text x="116" y="25.5" class="title" fill="var(--muted)">kp · your agent's memory, one local file</text>
  <g class="steps">{''.join(step_svg)}</g>

  <g class="term" transform="translate(30 0)">
    {chr(10).join('    ' + b for b in body)}
  </g>
</svg>"""


# Team sharing ------------------------------------------------------------


def semiring_svg() -> str:
    w, h = 1080, 516

    def panel(x: float, good: bool) -> str:
        accent = "var(--good)" if good else "var(--bad)"
        soft = "var(--good-soft)" if good else "var(--bad-soft)"
        label = "WITH KIPO" if good else "TYPICAL SHARED RAG"
        title = "Everyone sees only what they should" if good else "Private notes leak into the team index"
        people = [("A", "alice", True), ("B", "bob", True), ("C", "carol", False)]
        bullets = (
            [
                "Private notes stay in your local kp.sqlite.",
                "kp forget retracts it everywhere in 0 ms.",
                "Carol gets the same empty result as a missing note.",
            ]
            if good
            else [
                "Summaries inherit the widest access. Carol sees it.",
                "Retracted rules linger until someone re-indexes.",
                "Ex-teammates can still search old summaries.",
            ]
        )
        label_w = len(label) * 11.5 * 0.62 + 22
        out = [
            f'<g transform="translate({x} 96)">',
            f'<rect width="496" height="392" rx="16" fill="var(--panel)" stroke="var(--line)" stroke-width="1.5"/>',
            f'<rect x="20" y="0" width="456" height="3" rx="1.5" fill="{accent}"/>',
            f'<rect x="24" y="24" width="{label_w:.1f}" height="22" rx="11" fill="{soft}"/>',
            f'<text x="{24 + label_w / 2:.1f}" y="39" text-anchor="middle" class="mono" font-size="11.5" font-weight="700" fill="{accent}">{label}</text>',
            f'<text x="24" y="78" class="sans" font-size="19" font-weight="750" fill="var(--fg)">{title}</text>',
            '<rect x="24" y="96" width="448" height="44" rx="10" fill="var(--note)" stroke="var(--line)"/>',
            '<text x="40" y="123" class="mono" font-size="12.5" fill="var(--muted)">bob</text>',
            '<text x="76" y="123" class="sans" font-size="14" fill="var(--fg)">"DB freeze until Monday." Shared with alice and bob only.</text>',
        ]
        for j, (initial, name, allowed) in enumerate(people):
            cx = 98 + j * 150
            leaked = not allowed and not good
            ok = allowed or leaked
            ring = "var(--bad)" if leaked else ("var(--good)" if ok else "var(--line)")
            out.append(f'<path d="M248 140 C 248 158, {cx} 152, {cx} 170" fill="none" stroke="{ring}" stroke-width="1.6" stroke-dasharray="{"0" if ok else "4 4"}"/>')
            out.append(f'<circle cx="{cx}" cy="192" r="22" fill="var(--avatar)" stroke="{ring}" stroke-width="2.5"/>')
            out.append(f'<text x="{cx}" y="198" text-anchor="middle" class="sans" font-size="16" font-weight="800" fill="var(--fg)">{initial}</text>')
            out.append(f'<text x="{cx}" y="236" text-anchor="middle" class="sans" font-size="13" font-weight="600" fill="var(--fg)">{name}</text>')
            if leaked:
                status, fg, bg = "LEAKED", "var(--bad)", "var(--bad-soft)"
            elif ok:
                status, fg, bg = "SEES IT", "var(--good)", "var(--good-soft)"
            else:
                status, fg, bg = "NOTHING", "var(--muted)", "var(--note)"
            sw = len(status) * 10.5 * 0.62 + 18
            out.append(f'<rect x="{cx - sw / 2:.1f}" y="246" width="{sw:.1f}" height="20" rx="10" fill="{bg}"/>')
            out.append(f'<text x="{cx}" y="260" text-anchor="middle" class="mono" font-size="10.5" font-weight="700" fill="{fg}">{status}</text>')
        out.append('<line x1="24" y1="286" x2="472" y2="286" stroke="var(--line)"/>')
        for k, line in enumerate(bullets):
            by = 312 + k * 26
            if good:
                glyph = f'<path d="M{29} {by - 5} l3.5 3.5 l6.5 -7" fill="none" stroke="{accent}" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/>'
            else:
                glyph = f'<path d="M{30} {by - 9} l7 7 M{37} {by - 9} l-7 7" stroke="{accent}" stroke-width="2.2" stroke-linecap="round"/>'
            out.append(glyph)
            out.append(f'<text x="50" y="{by}" class="sans" font-size="14.5" fill="var(--fg)">{t(line)}</text>')
        out.append("</g>")
        return "\n    ".join(out)

    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="100%">
  <style>
    :root {{
      --bg: #FBFCFD; --panel: #FFFFFF; --note: #F3F5F8; --avatar: #EEF1F5; --line: #DCE2EA;
      --fg: #0B1220; --muted: #5A6678;
      --bad: #D93636; --bad-soft: #FDE8E8; --good: #3B7D0C; --good-soft: #EEF9DD;
    }}
    @media (prefers-color-scheme: dark) {{
      :root {{
        --bg: {INK}; --panel: {INK_RAISED}; --note: #182133; --avatar: #1A2436; --line: {INK_LINE};
        --fg: {PAPER}; --muted: {MUTED};
        --bad: {RED}; --bad-soft: #33161A; --good: {LIME}; --good-soft: #1C2A12;
      }}
    }}
    .sans {{ font-family: {SANS}; }}
    .mono {{ font-family: {MONO}; }}
  </style>
  <rect width="{w}" height="{h}" rx="18" fill="var(--bg)"/>
  <rect x="0.75" y="0.75" width="{w - 1.5}" height="{h - 1.5}" rx="17.25" fill="none" stroke="var(--line)" stroke-width="1.5"/>
  <text x="{w / 2}" y="50" text-anchor="middle" class="sans" font-size="26" font-weight="800" letter-spacing="-0.5" fill="var(--fg)">Share with your team. Leak nothing.</text>
  <text x="{w / 2}" y="76" text-anchor="middle" class="sans" font-size="15" fill="var(--muted)">One note from bob, meant for alice and bob. Who can read it?</text>
  {panel(28, False)}
  {panel(556, True)}
</svg>"""


# Workflow map ------------------------------------------------------------


def architecture_svg() -> str:
    w, h = 1080, 540
    sources = ["Claude Code", "Codex", "Cursor", "Windsurf", "Code and docs", "PDF and Office files", "Web pages", "GitHub and team chat"]
    outputs = [
        ("Recall in every prompt", "≤ 250 tokens, ≤ 3 items", "var(--cyan)"),
        ("Live command guardrails", "PreToolUse ask or deny", "var(--lime)"),
        ("Skills built overnight", "kp dream → SKILL.md", "var(--amber)"),
        ("Optional team sync", "kp publish · kp sync", "var(--violet)"),
    ]
    vault_x, vault_y, vault_w, vault_h = 384, 150, 312, 296
    vault_cy = vault_y + vault_h / 2
    left = []
    for i, name in enumerate(sources):
        y = 132 + i * 41
        left.append(f'<rect x="28" y="{y}" width="248" height="33" rx="9" fill="var(--card)" stroke="var(--line)"/>')
        left.append(f'<circle cx="46" cy="{y + 16.5}" r="4" fill="var(--muted)"/>')
        left.append(f'<text x="60" y="{y + 21.5}" class="sans" font-size="14" font-weight="600" fill="var(--fg)">{t(name)}</text>')
        sy = y + 16.5
        ty = vault_cy - 60 + i * (120 / 7)
        left.append(f'<path d="M276 {sy} C 330 {sy}, 330 {ty:.1f}, {vault_x} {ty:.1f}" fill="none" stroke="var(--wire)" stroke-width="1.5"/>')
    right = []
    for i, (title, sub, color) in enumerate(outputs):
        y = 132 + i * 82
        right.append(f'<rect x="804" y="{y}" width="248" height="70" rx="12" fill="var(--card)" stroke="var(--line)"/>')
        right.append(f'<rect x="804" y="{y + 14}" width="4" height="42" rx="2" fill="{color}"/>')
        right.append(f'<text x="824" y="{y + 30}" class="sans" font-size="15" font-weight="750" fill="var(--fg)">{t(title)}</text>')
        right.append(f'<text x="824" y="{y + 52}" class="mono" font-size="12.5" fill="{color}">{t(sub)}</text>')
        ty = y + 35
        sy = vault_cy - 45 + i * 30
        right.append(f'<path d="M{vault_x + vault_w} {sy} C 750 {sy}, 750 {ty}, 804 {ty}" fill="none" stroke="{color}" stroke-width="1.8" stroke-opacity="0.8"/>')
        right.append(f'<circle cx="804" cy="{ty}" r="3.5" fill="{color}"/>')
    checks = ["Secrets scrubbed before saving", "Works fully offline", "See everything with kp inspect"]
    check_svg = []
    for i, line in enumerate(checks):
        cy = vault_y + 206 + i * 26
        check_svg.append(f'<path d="M{vault_x + 46} {cy - 5} l3.5 3.5 l6.5 -7" fill="none" stroke="var(--lime)" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/>')
        check_svg.append(f'<text x="{vault_x + 66}" y="{cy}" class="sans" font-size="14" fill="var(--fg)">{t(line)}</text>')

    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="100%">
  <style>
    :root {{
      --bg: #FBFCFD; --card: #FFFFFF; --vault: #FFFFFF; --line: #DCE2EA; --wire: #C3CCD8;
      --fg: #0B1220; --muted: #5A6678;
      --cyan: #0B7BC0; --lime: #3B7D0C; --amber: #B7791F; --violet: #6D4AE0;
    }}
    @media (prefers-color-scheme: dark) {{
      :root {{
        --bg: {INK}; --card: {INK_RAISED}; --vault: #101827; --line: {INK_LINE}; --wire: #34415C;
        --fg: {PAPER}; --muted: {MUTED};
        --cyan: {CYAN}; --lime: {LIME}; --amber: {AMBER}; --violet: #A894FF;
      }}
    }}
    .sans {{ font-family: {SANS}; }}
    .mono {{ font-family: {MONO}; }}
  </style>
  <rect width="{w}" height="{h}" rx="18" fill="var(--bg)"/>
  <rect x="0.75" y="0.75" width="{w - 1.5}" height="{h - 1.5}" rx="17.25" fill="none" stroke="var(--line)" stroke-width="1.5"/>
  <text x="{w / 2}" y="50" text-anchor="middle" class="sans" font-size="26" font-weight="800" letter-spacing="-0.5" fill="var(--fg)">How Kipo fits your daily workflow</text>
  <text x="{w / 2}" y="76" text-anchor="middle" class="sans" font-size="15" fill="var(--muted)">The tools you already use write to one local file. Your agent reads back only what helps.</text>

  <text x="28" y="118" class="mono" font-size="11.5" font-weight="700" letter-spacing="1" fill="var(--muted)">YOU WORK IN</text>
  <text x="804" y="118" class="mono" font-size="11.5" font-weight="700" letter-spacing="1" fill="var(--muted)">YOUR AGENT GETS</text>
  {chr(10).join('  ' + s for s in left)}
  {chr(10).join('  ' + s for s in right)}

  <rect x="{vault_x}" y="{vault_y}" width="{vault_w}" height="{vault_h}" rx="22" fill="var(--vault)" stroke="var(--lime)" stroke-width="2"/>
  {mark(vault_x + vault_w / 2 - 36, vault_y + 30, 72, 'arch')}
  <text x="{vault_x + vault_w / 2}" y="{vault_y + 146}" text-anchor="middle" class="mono" font-size="28" font-weight="800" fill="var(--fg)">kp.sqlite</text>
  <text x="{vault_x + vault_w / 2}" y="{vault_y + 172}" text-anchor="middle" class="sans" font-size="14" fill="var(--muted)">one file on your machine</text>
  {''.join(check_svg)}

  <text x="{w / 2}" y="{h - 30}" text-anchor="middle" class="mono" font-size="12.5" fill="var(--muted)">Local by default  ·  no cloud account  ·  Apache-2.0</text>
</svg>"""


# Video poster ------------------------------------------------------------


def video_poster_svg() -> str:
    w, h = 1080, 240
    chip = "PRODUCT TRAILER · ~1 MIN · HD AUDIO"
    chip_w = len(chip) * 12 * 0.62 + 24
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="100%">
  <defs>
    <radialGradient id="poster-glow" cx="0.1" cy="0.5" r="0.45">
      <stop offset="0" stop-color="{LIME}" stop-opacity="0.22"/>
      <stop offset="1" stop-color="{LIME}" stop-opacity="0"/>
    </radialGradient>
  </defs>
  {style()}
  <rect width="{w}" height="{h}" rx="18" fill="{INK}"/>
  <rect width="{w}" height="{h}" rx="18" fill="url(#poster-glow)"/>
  <rect x="0.75" y="0.75" width="{w - 1.5}" height="{h - 1.5}" rx="17.25" fill="none" stroke="{INK_LINE}" stroke-width="1.5"/>

  <circle cx="120" cy="120" r="66" fill="none" stroke="{LIME}" stroke-opacity="0.25" stroke-width="2"/>
  <circle cx="120" cy="120" r="54" fill="{LIME}"/>
  <path d="M106 92 L106 148 L150 120 Z" fill="{INK}" stroke="{INK}" stroke-width="4" stroke-linejoin="round"/>

  <rect x="224" y="40" width="{chip_w:.1f}" height="26" rx="13" fill="#1C2A12" stroke="{LIME}" stroke-opacity="0.5"/>
  <text x="{224 + chip_w / 2:.1f}" y="57.5" text-anchor="middle" class="mono" font-size="12" font-weight="700" fill="{LIME}">{t(chip)}</text>
  <text x="224" y="112" class="sans" font-size="32" font-weight="800" letter-spacing="-0.6" fill="{PAPER}">See how Kipo stops AI amnesia</text>
  <text x="224" y="150" class="sans" font-size="32" font-weight="800" letter-spacing="-0.6" fill="{PAPER}">and dangerous commands.</text>
  <text x="224" y="192" class="mono" font-size="13.5" fill="{MUTED}">Click to play the MP4  ·  or run kp demo for the 5-minute offline tour</text>

  {mark(880, 52, 136, 'poster')}
</svg>"""


# Social preview ----------------------------------------------------------


def social_preview_svg() -> str:
    w, h = 1280, 640
    chips = [("≤ 250-token recall", CYAN), ("live command shield", LIME), ("overnight SKILL.md", AMBER)]
    chip_svg = []
    x = 96
    for label, color in chips:
        cw = len(label) * 17 * 0.6 + 36
        chip_svg.append(
            f'<rect x="{x}" y="452" width="{cw:.1f}" height="44" rx="22" fill="none" stroke="{color}" stroke-opacity="0.6" stroke-width="1.5"/>'
            f'<text x="{x + cw / 2:.1f}" y="480" text-anchor="middle" class="mono" font-size="17" font-weight="700" fill="{color}">{t(label)}</text>'
        )
        x += cw + 16
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}">
  <defs>
    <radialGradient id="social-glow" cx="0.82" cy="0.18" r="0.7">
      <stop offset="0" stop-color="{LIME}" stop-opacity="0.2"/>
      <stop offset="1" stop-color="{LIME}" stop-opacity="0"/>
    </radialGradient>
    <pattern id="social-grid" width="40" height="40" patternUnits="userSpaceOnUse">
      <path d="M40 0 H0 V40" fill="none" stroke="#FFFFFF" stroke-opacity="0.035" stroke-width="1"/>
    </pattern>
  </defs>
  {style()}
  <rect width="{w}" height="{h}" fill="{INK}"/>
  <rect width="{w}" height="{h}" fill="url(#social-grid)"/>
  <rect width="{w}" height="{h}" fill="url(#social-glow)"/>

  {mark(96, 96, 168, 'social')}
  <text x="296" y="222" class="sans" font-size="128" font-weight="800" letter-spacing="-5" fill="{PAPER}">Kipo</text>
  <rect x="574" y="132" width="104" height="62" rx="14" fill="#18233A" stroke="{INK_LINE}" stroke-width="2"/>
  <text x="626" y="176" text-anchor="middle" class="mono" font-size="34" font-weight="800" fill="{LIME}">kp</text>

  <g transform="translate(860 96)">
    <rect width="324" height="200" rx="18" fill="{INK_RAISED}" stroke="{INK_LINE}" stroke-width="1.5"/>
    <circle cx="24" cy="24" r="5" fill="#FF5F57"/><circle cx="42" cy="24" r="5" fill="#FEBC2E"/><circle cx="60" cy="24" r="5" fill="#28C840"/>
    <text x="24" y="72" class="mono" font-size="15" fill="{PAPER}"><tspan fill="{LIME}">$</tspan> kp connect claude-code</text>
    <text x="24" y="102" class="mono" font-size="15" fill="{CYAN}">recalled 2 memories, 186 tok</text>
    <text x="24" y="132" class="mono" font-size="15" fill="{RED}">✕ blocked sqlx migrate run</text>
    <text x="24" y="162" class="mono" font-size="15" fill="{PAPER}"><tspan fill="{LIME}">$</tspan> kp forget <tspan fill="{MUTED}">…</tspan> <tspan fill="{LIME}">0 ms</tspan></text>
  </g>
  <text x="96" y="348" class="sans" font-size="54" font-weight="800" letter-spacing="-1.5" fill="{PAPER}">Your coding agent forgets.</text>
  <text x="96" y="412" class="sans" font-size="54" font-weight="800" letter-spacing="-1.5" fill="{LIME}">Kipo remembers, and guards.</text>
  {''.join(chip_svg)}
  <text x="96" y="568" class="mono" font-size="19" fill="{MUTED}">Claude Code · Codex · Cursor · Windsurf  ·  one local kp.sqlite  ·  no cloud</text>
</svg>"""


def main() -> None:
    write_svg(ASSETS / "kipo-logo.svg", logo_svg())
    write_svg(ASSETS / "kipo-hero-dark.svg", hero_svg(dark=True))
    write_svg(ASSETS / "kipo-hero-light.svg", hero_svg(dark=False))
    write_svg(ASSETS / "kipo-terminal-demo.svg", terminal_demo_svg())
    write_svg(ASSETS / "kipo-semiring.svg", semiring_svg())
    write_svg(ASSETS / "kipo-architecture.svg", architecture_svg())
    write_svg(ASSETS / "kipo-video-poster.svg", video_poster_svg())
    write_svg(ASSETS / "kipo-social-preview.svg", social_preview_svg())
    print(f"Generated 8 verified SVG assets in {ASSETS.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
