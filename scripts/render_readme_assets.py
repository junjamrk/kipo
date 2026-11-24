#!/usr/bin/env python3
"""Deterministically generate Kipo SVG visual assets under docs/assets/."""

from pathlib import Path
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "docs" / "assets"

FONT_SANS = 'ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif'
FONT_MONO = 'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace'


def write_svg(path: Path, content: str) -> None:
    text = content.strip() + "\n"
    root = ET.fromstring(text)
    if not root.tag.endswith("svg"):
        raise ValueError(f"{path}: root element is not <svg>")
    if "viewBox" not in root.attrib:
        raise ValueError(f"{path}: missing viewBox attribute")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def hero_svg(dark: bool) -> str:
    if dark:
        bg = "#0B111B"
        card = "#121B2A"
        stroke = "#24324A"
        fg = "#F0F6FC"
        muted = "#94A3B8"
        accent = "#38BDF8"
        accent_soft = "#0F2942"
        green = "#34D399"
        green_soft = "#0D2E27"
        amber = "#FBBF24"
        badge_bg = "#172336"
    else:
        bg = "#F8FAFC"
        card = "#FFFFFF"
        stroke = "#CBD5E1"
        fg = "#0F172A"
        muted = "#475569"
        accent = "#0284C7"
        accent_soft = "#E0F2FE"
        green = "#059669"
        green_soft = "#D1FAE5"
        amber = "#D97706"
        badge_bg = "#EFF6FF"

    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 290" width="100%">
  <defs>
    <style>
      .sans {{ font-family: {FONT_SANS}; }}
      .mono {{ font-family: {FONT_MONO}; }}
    </style>
  </defs>
  <rect width="1200" height="290" rx="18" fill="{bg}" stroke="{stroke}" stroke-width="1.5"/>

  <g transform="translate(48, 42)">
    <rect width="68" height="68" rx="16" fill="{accent_soft}" stroke="{accent}" stroke-width="2"/>
    <circle cx="24" cy="24" r="6" fill="{accent}"/>
    <circle cx="46" cy="22" r="5" fill="{green}"/>
    <circle cx="35" cy="46" r="6" fill="{amber}"/>
    <line x1="24" y1="24" x2="46" y2="22" stroke="{accent}" stroke-width="2"/>
    <line x1="24" y1="24" x2="35" y2="46" stroke="{accent}" stroke-width="2"/>
    <line x1="46" y1="22" x2="35" y2="46" stroke="{green}" stroke-width="2"/>

    <text x="88" y="36" class="sans" font-size="34" font-weight="800" fill="{fg}">Kipo</text>
    <rect x="176" y="10" width="58" height="32" rx="8" fill="{badge_bg}" stroke="{stroke}" stroke-width="1"/>
    <text x="205" y="31" text-anchor="middle" class="mono" font-size="18" font-weight="700" fill="{accent}">kp</text>

    <text x="88" y="64" class="sans" font-size="17" font-weight="500" fill="{muted}">
      Local-first personal and team memory database for AI coding agents, backed by one SQLite file.
    </text>
  </g>

  <g transform="translate(48, 138)">
    <rect x="0" y="0" width="252" height="108" rx="12" fill="{card}" stroke="{stroke}" stroke-width="1.5"/>
    <text x="20" y="30" class="mono" font-size="12" font-weight="700" fill="{accent}">01  CAPTURE &amp; INGEST</text>
    <text x="20" y="55" class="sans" font-size="15" font-weight="700" fill="{fg}">Non-blocking agent hooks</text>
    <text x="20" y="77" class="sans" font-size="13" fill="{muted}">Claude Code, Codex, MCP, files,</text>
    <text x="20" y="95" class="sans" font-size="13" fill="{muted}">GitHub, and Feishu via JSONL</text>

    <path d="M 258 54 L 284 54" stroke="{accent}" stroke-width="2" fill="none"/>
    <polygon points="284,49 294,54 284,59" fill="{accent}"/>

    <rect x="300" y="0" width="268" height="108" rx="12" fill="{card}" stroke="{stroke}" stroke-width="1.5"/>
    <text x="320" y="30" class="mono" font-size="12" font-weight="700" fill="{green}">02  SINGLE-FILE ENGINE</text>
    <text x="320" y="55" class="sans" font-size="15" font-weight="700" fill="{fg}">Isomorphic kp.sqlite (WAL)</text>
    <text x="320" y="77" class="sans" font-size="13" fill="{muted}">Redacted L0 blobs, CJK FTS5,</text>
    <text x="320" y="95" class="sans" font-size="13" fill="{muted}">local ONNX vectors, bitemporal L1</text>

    <path d="M 574 54 L 600 54" stroke="{green}" stroke-width="2" fill="none"/>
    <polygon points="600,49 610,54 600,59" fill="{green}"/>

    <rect x="616" y="0" width="252" height="108" rx="12" fill="{card}" stroke="{stroke}" stroke-width="1.5"/>
    <text x="636" y="30" class="mono" font-size="12" font-weight="700" fill="{amber}">03  PROVENANCE SEMIRING</text>
    <text x="636" y="55" class="sans" font-size="15" font-weight="700" fill="{fg}">Read-time visibility proofs</text>
    <text x="636" y="77" class="sans" font-size="13" fill="{muted}">Zero copied ACLs; instant forget,</text>
    <text x="636" y="95" class="sans" font-size="13" fill="{muted}">expiry, and roster contraction</text>

    <path d="M 874 54 L 900 54" stroke="{amber}" stroke-width="2" fill="none"/>
    <polygon points="900,49 910,54 900,59" fill="{amber}"/>

    <rect x="916" y="0" width="188" height="108" rx="12" fill="{green_soft}" stroke="{green}" stroke-width="1.5"/>
    <text x="934" y="30" class="mono" font-size="12" font-weight="700" fill="{green}">04  GUARDED ACTION</text>
    <text x="934" y="55" class="sans" font-size="15" font-weight="700" fill="{fg}">&lt;= 250-tok recall</text>
    <text x="934" y="77" class="sans" font-size="13" fill="{muted}">PreToolUse guards,</text>
    <text x="934" y="95" class="sans" font-size="13" fill="{muted}">why proofs, kp dream</text>
  </g>
</svg>"""


def terminal_demo_svg() -> str:
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 980 470" width="100%">
  <style>
    :root {{
      --bg: #0D1117;
      --chrome: #161B22;
      --border: #30363D;
      --fg: #E6EDF3;
      --muted: #8B949E;
      --cyan: #58A6FF;
      --green: #3FB950;
      --amber: #D29922;
      --red: #F85149;
      --chip: #1F2937;
    }}
    @media (prefers-color-scheme: light) {{
      :root {{
        --bg: #0F172A;
        --chrome: #1E293B;
        --border: #334155;
        --fg: #F8FAFC;
        --muted: #94A3B8;
        --cyan: #38BDF8;
        --green: #34D399;
        --amber: #FBBF24;
        --red: #FB7185;
        --chip: #1E293B;
      }}
    }}
    .mono {{ font-family: {FONT_MONO}; font-size: 13px; }}
    .title {{ font-family: {FONT_SANS}; font-size: 12.5px; font-weight: 600; }}
    .s1 {{ opacity: 1; }}
    .s2 {{ animation: reveal2 14s infinite; }}
    .s3 {{ animation: reveal3 14s infinite; }}
    .s4 {{ animation: reveal4 14s infinite; }}
    .cursor {{ animation: blink 1s steps(2, end) infinite; }}
    @keyframes reveal2 {{
      0%, 16% {{ opacity: 0; }}
      22%, 95% {{ opacity: 1; }}
      99%, 100% {{ opacity: 0; }}
    }}
    @keyframes reveal3 {{
      0%, 42% {{ opacity: 0; }}
      48%, 95% {{ opacity: 1; }}
      99%, 100% {{ opacity: 0; }}
    }}
    @keyframes reveal4 {{
      0%, 68% {{ opacity: 0; }}
      74%, 95% {{ opacity: 1; }}
      99%, 100% {{ opacity: 0; }}
    }}
    @keyframes blink {{
      0%, 49% {{ opacity: 1; }}
      50%, 100% {{ opacity: 0; }}
    }}
  </style>

  <rect width="980" height="470" rx="14" fill="var(--bg)" stroke="var(--border)" stroke-width="1.5"/>
  <path d="M 1 14 Q 1 1 14 1 L 966 1 Q 979 1 979 14 L 979 40 L 1 40 Z" fill="var(--chrome)"/>
  <line x1="0" y1="40" x2="980" y2="40" stroke="var(--border)" stroke-width="1"/>
  <circle cx="22" cy="20" r="6" fill="#FF5F56"/>
  <circle cx="42" cy="20" r="6" fill="#FFBD2E"/>
  <circle cx="62" cy="20" r="6" fill="#27C93F"/>
  <text x="490" y="25" text-anchor="middle" class="title" fill="var(--muted)">kp - personal &amp; team memory loop (100% local SQLite)</text>

  <g transform="translate(28, 70)" class="mono">
    <g class="s1">
      <text x="0" y="0"><tspan fill="var(--green)">$</tspan> <tspan fill="var(--fg)">kp init --path ~/.kp/personal --owner alice &amp;&amp; kp connect claude-code --project .</tspan></text>
      <text x="0" y="24" fill="var(--muted)">Initialized personal library (local:bge-small-zh-v1.5, 512d) · Installed async hooks</text>

      <text x="0" y="58"><tspan fill="var(--green)">$</tspan> <tspan fill="var(--fg)">kp remember --subject "prod-db" --predicate kp.ops.freeze \\</tspan></text>
      <text x="0" y="80" fill="var(--fg)">    --value "Release freeze until Monday 09:00 UTC" --kind constraint \\</text>
      <text x="0" y="102" fill="var(--fg)">    --tags "release,[forbid: sqlx migrate]" --logical-key release-freeze --audience alice,bob</text>
    </g>

    <g class="s2">
      <text x="0" y="140"><tspan fill="var(--cyan)">[UserPromptSubmit &lt;= 250 tok]</tspan> <tspan fill="var(--muted)">Prompt: "Roll out billing schema migration"</tspan></text>
      <text x="0" y="164" fill="var(--green)">  &lt;kp-context&gt; [constraint] prod-db: Release freeze until Monday 09:00 UTC [kp-ref:a81f29c4]</text>
      <text x="0" y="186" fill="var(--muted)">  &#8627; author=alice · audience=&#123;alice,bob&#125; · active validity window</text>
    </g>

    <g class="s3">
      <text x="0" y="226"><tspan fill="var(--amber)">[PreToolUse Guard]</tspan> <tspan fill="var(--fg)">Tool: Bash("sqlx migrate run")</tspan></text>
      <text x="0" y="250" fill="var(--red)">  blocked -&gt; Claude Code: permissionDecision="ask" | Codex: permissionDecision="deny"</text>
      <text x="0" y="272" fill="var(--muted)">  reason: matches [forbid: sqlx migrate] on [kp-ref:a81f29c4] (override: kp allow kp-ref:a81f29c4)</text>
    </g>

    <g class="s4">
      <text x="0" y="312"><tspan fill="var(--green)">$</tspan> <tspan fill="var(--fg)">kp forget release-freeze &amp;&amp; kp search "sqlx migrate" --as carol --json</tspan></text>
      <text x="0" y="336" fill="var(--green)">  withdrew release-freeze -&gt; dependent warnings invalidated · carol results: [] (zero leakage)</text>
      <text x="0" y="370"><tspan fill="var(--green)">$</tspan> <tspan fill="var(--fg)">kp demo</tspan></text>
      <rect x="86" y="356" width="9" height="17" fill="var(--cyan)" class="cursor"/>
    </g>
  </g>
</svg>"""


def semiring_svg() -> str:
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1080 390" width="100%">
  <style>
    :root {{
      --bg: #FFFFFF;
      --panel-bad: #FEF2F2;
      --stroke-bad: #FCA5A5;
      --title-bad: #B91C1C;
      --panel-good: #ECFDF5;
      --stroke-good: #6EE7B7;
      --title-good: #047857;
      --card: #FFFFFF;
      --stroke: #CBD5E1;
      --fg: #0F172A;
      --muted: #475569;
      --accent: #0284C7;
    }}
    @media (prefers-color-scheme: dark) {{
      :root {{
        --bg: #0B111B;
        --panel-bad: #1F1218;
        --stroke-bad: #7F1D1D;
        --title-bad: #FCA5A5;
        --panel-good: #0D231E;
        --stroke-good: #065F46;
        --title-good: #6EE7B7;
        --card: #131D2E;
        --stroke: #2A3B54;
        --fg: #F1F5F9;
        --muted: #94A3B8;
        --accent: #38BDF8;
      }}
    }}
    .sans {{ font-family: {FONT_SANS}; }}
    .mono {{ font-family: {FONT_MONO}; }}
  </style>

  <rect width="1080" height="390" rx="16" fill="var(--bg)" stroke="var(--stroke)" stroke-width="1.5"/>

  <g transform="translate(28, 26)">
    <rect width="496" height="338" rx="14" fill="var(--panel-bad)" stroke="var(--stroke-bad)" stroke-width="1.5"/>
    <text x="24" y="36" class="sans" font-size="18" font-weight="800" fill="var(--title-bad)">Conventional shared memory (copied ACLs)</text>
    <text x="24" y="60" class="sans" font-size="13" fill="var(--muted)">Permissions and validity are baked into summaries at write time.</text>

    <rect x="24" y="84" width="208" height="72" rx="10" fill="var(--card)" stroke="var(--stroke)"/>
    <text x="38" y="110" class="mono" font-size="12" font-weight="700" fill="var(--fg)">Premise A (Event)</text>
    <text x="38" y="132" class="mono" font-size="11.5" fill="var(--muted)">aud = &#123;alice, bob, carol&#125;</text>

    <rect x="264" y="84" width="208" height="72" rx="10" fill="var(--card)" stroke="var(--stroke)"/>
    <text x="278" y="110" class="mono" font-size="12" font-weight="700" fill="var(--fg)">Premise B (Constraint)</text>
    <text x="278" y="132" class="mono" font-size="11.5" fill="var(--muted)">aud = &#123;alice, bob&#125;</text>

    <line x1="128" y1="156" x2="228" y2="196" stroke="var(--title-bad)" stroke-width="2"/>
    <line x1="368" y1="156" x2="268" y2="196" stroke="var(--title-bad)" stroke-width="2"/>

    <rect x="24" y="196" width="448" height="118" rx="10" fill="var(--card)" stroke="var(--stroke-bad)" stroke-width="1.5"/>
    <text x="42" y="224" class="sans" font-size="13.5" font-weight="700" fill="var(--title-bad)">Materialized summary chunk in vector index</text>
    <text x="42" y="248" class="sans" font-size="12" fill="var(--fg)">1. Summary inherits broad channel ACL, leaking B to carol.</text>
    <text x="42" y="270" class="sans" font-size="12" fill="var(--fg)">2. When bob leaves or withdraws B, stale summary vectors</text>
    <text x="42" y="290" class="sans" font-size="12" fill="var(--fg)">   keep surfacing in prompts until manual re-indexing.</text>
  </g>

  <g transform="translate(556, 26)">
    <rect width="496" height="338" rx="14" fill="var(--panel-good)" stroke="var(--stroke-good)" stroke-width="1.5"/>
    <text x="24" y="36" class="sans" font-size="18" font-weight="800" fill="var(--title-good)">Kipo read-time provenance semiring</text>
    <text x="24" y="60" class="sans" font-size="12.5" fill="var(--muted)">Derived facts store audience = "__semiring__" and evaluate on read.</text>

    <rect x="24" y="84" width="208" height="72" rx="10" fill="var(--card)" stroke="var(--stroke)"/>
    <text x="38" y="110" class="mono" font-size="12" font-weight="700" fill="var(--fg)">Premise A @ [t1, t2)</text>
    <text x="38" y="132" class="mono" font-size="11.5" fill="var(--muted)">aud = &#123;alice, bob, carol&#125;</text>

    <rect x="264" y="84" width="208" height="72" rx="10" fill="var(--card)" stroke="var(--stroke)"/>
    <text x="278" y="110" class="mono" font-size="12" font-weight="700" fill="var(--fg)">Premise B @ [t1, inf)</text>
    <text x="278" y="132" class="mono" font-size="11.5" fill="var(--muted)">aud = &#123;alice, bob&#125;</text>

    <line x1="128" y1="156" x2="228" y2="196" stroke="var(--title-good)" stroke-width="2"/>
    <line x1="368" y1="156" x2="268" y2="196" stroke="var(--title-good)" stroke-width="2"/>

    <rect x="24" y="196" width="448" height="118" rx="10" fill="var(--card)" stroke="var(--stroke-good)" stroke-width="1.5"/>
    <text x="42" y="224" class="mono" font-size="12" font-weight="700" fill="var(--title-good)">visible(v, t) = OR_proof ( AND_p visible(p, v, t) )</text>
    <text x="42" y="248" class="sans" font-size="12" fill="var(--fg)">1. Effective visibility is &#123;alice, bob&#125; at [t1, t2). Carol gets</text>
    <text x="42" y="268" class="sans" font-size="12" fill="var(--fg)">   byte-identical 404 and search results with zero leakage.</text>
    <text x="42" y="288" class="sans" font-size="12" fill="var(--fg)">2. kp forget, clock expiry, or roster change takes 0 ms.</text>
  </g>
</svg>"""


def architecture_svg() -> str:
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1080 500" width="100%">
  <style>
    :root {{
      --bg: #FFFFFF;
      --layer: #F8FAFC;
      --card: #FFFFFF;
      --stroke: #CBD5E1;
      --fg: #0F172A;
      --muted: #475569;
      --blue: #0284C7;
      --green: #059669;
      --amber: #D97706;
      --purple: #7C3AED;
    }}
    @media (prefers-color-scheme: dark) {{
      :root {{
        --bg: #0B111B;
        --layer: #111927;
        --card: #172235;
        --stroke: #293A52;
        --fg: #F1F5F9;
        --muted: #94A3B8;
        --blue: #38BDF8;
        --green: #34D399;
        --amber: #FBBF24;
        --purple: #A78BFA;
      }}
    }}
    .sans {{ font-family: {FONT_SANS}; }}
    .mono {{ font-family: {FONT_MONO}; }}
  </style>

  <rect width="1080" height="500" rx="16" fill="var(--bg)" stroke="var(--stroke)" stroke-width="1.5"/>

  <g transform="translate(28, 24)">
    <rect width="1024" height="86" rx="12" fill="var(--layer)" stroke="var(--stroke)"/>
    <text x="20" y="26" class="mono" font-size="11.5" font-weight="700" fill="var(--blue)">PRODUCERS &amp; AGENT HOOKS (EQUAL EXTERNAL CALLERS)</text>

    <rect x="20" y="38" width="232" height="36" rx="8" fill="var(--card)" stroke="var(--stroke)"/>
    <text x="136" y="61" text-anchor="middle" class="sans" font-size="13" font-weight="600" fill="var(--fg)">Claude Code &amp; Codex Hooks</text>

    <rect x="270" y="38" width="232" height="36" rx="8" fill="var(--card)" stroke="var(--stroke)"/>
    <text x="386" y="61" text-anchor="middle" class="sans" font-size="13" font-weight="600" fill="var(--fg)">kp-connector (GitHub / Feishu)</text>

    <rect x="520" y="38" width="232" height="36" rx="8" fill="var(--card)" stroke="var(--stroke)"/>
    <text x="636" y="61" text-anchor="middle" class="sans" font-size="12" font-weight="600" fill="var(--fg)">kp add (Code, PDF, Office, Web)</text>

    <rect x="770" y="38" width="234" height="36" rx="8" fill="var(--card)" stroke="var(--stroke)"/>
    <text x="887" y="61" text-anchor="middle" class="sans" font-size="13" font-weight="600" fill="var(--fg)">CLI kp ingest · MCP kp_note</text>
  </g>

  <g transform="translate(28, 128)">
    <rect width="1024" height="62" rx="12" fill="var(--layer)" stroke="var(--blue)" stroke-width="1.5"/>
    <text x="512" y="27" text-anchor="middle" class="mono" font-size="12.5" font-weight="700" fill="var(--blue)">GOVERNED INGEST &amp; PRIVACY BOUNDARY (Draft 2020-12 JSON Schema · SecretRedactor · RawOnly Gate)</text>
    <text x="512" y="47" text-anchor="middle" class="sans" font-size="12.5" fill="var(--muted)">Pre-persistence credential scrubbing · Closed unknown-field rejection · Monotonic container roster ceilings · Private 0700 UDS daemon</text>
  </g>

  <g transform="translate(28, 208)">
    <rect width="1024" height="152" rx="12" fill="var(--layer)" stroke="var(--green)" stroke-width="1.5"/>
    <text x="20" y="26" class="mono" font-size="12" font-weight="700" fill="var(--green)">ISOMORPHIC SINGLE-FILE SQLITE ENGINE (kp.sqlite WAL — PERSONAL LIBRARY = 1-MEMBER COMMUNITY)</text>

    <rect x="20" y="42" width="236" height="94" rx="10" fill="var(--card)" stroke="var(--stroke)"/>
    <text x="36" y="66" class="mono" font-size="12.5" font-weight="700" fill="var(--fg)">L0 Verbatim Storage</text>
    <text x="36" y="88" class="sans" font-size="12" fill="var(--muted)">raw_blobs + raw_sources</text>
    <text x="36" y="106" class="sans" font-size="12" fill="var(--muted)">tree-sitter &amp; page anchors</text>
    <text x="36" y="124" class="sans" font-size="12" fill="var(--muted)">HMAC tombstones + secure_delete</text>

    <rect x="270" y="42" width="236" height="94" rx="10" fill="var(--card)" stroke="var(--stroke)"/>
    <text x="286" y="66" class="mono" font-size="12.5" font-weight="700" fill="var(--fg)">Hybrid Scoped Indexes</text>
    <text x="286" y="88" class="sans" font-size="12" fill="var(--muted)">jieba-rs CJK/EN FTS5 BM25</text>
    <text x="286" y="106" class="sans" font-size="12" fill="var(--muted)">Local ONNX + 8 cloud recipes</text>
    <text x="286" y="124" class="sans" font-size="12" fill="var(--muted)">RRF fusion + DPP diversity</text>

    <rect x="520" y="42" width="236" height="94" rx="10" fill="var(--card)" stroke="var(--stroke)"/>
    <text x="536" y="66" class="mono" font-size="12.5" font-weight="700" fill="var(--fg)">L1 &amp; Semiring Proofs</text>
    <text x="536" y="88" class="sans" font-size="12" fill="var(--muted)">Bitemporal facts &amp; slots</text>
    <text x="536" y="106" class="sans" font-size="12" fill="var(--muted)">Datalog collisions &amp; JTMS</text>
    <text x="536" y="124" class="sans" font-size="12" fill="var(--muted)">justifications proof DAG</text>

    <rect x="770" y="42" width="234" height="94" rx="10" fill="var(--card)" stroke="var(--stroke)"/>
    <text x="786" y="66" class="mono" font-size="12.5" font-weight="700" fill="var(--fg)">Sleep-Time RSI (kp dream)</text>
    <text x="786" y="88" class="sans" font-size="12" fill="var(--muted)">Contrastive episode mining</text>
    <text x="786" y="106" class="sans" font-size="12" fill="var(--muted)">CommandGuardSpec extraction</text>
    <text x="786" y="124" class="sans" font-size="12" fill="var(--muted)">k-of-N SKILL.md promotion</text>
  </g>

  <g transform="translate(28, 378)">
    <rect width="1024" height="96" rx="12" fill="var(--layer)" stroke="var(--purple)" stroke-width="1.5"/>
    <text x="20" y="26" class="mono" font-size="11.5" font-weight="700" fill="var(--purple)">VIEWER-ISOLATED READ, GOVERNANCE &amp; COLLABORATION INTERFACES</text>

    <rect x="20" y="40" width="236" height="42" rx="8" fill="var(--card)" stroke="var(--stroke)"/>
    <text x="138" y="66" text-anchor="middle" class="sans" font-size="12.5" font-weight="600" fill="var(--fg)">&lt;=250-tok Prompt &amp; PreToolUse</text>

    <rect x="270" y="40" width="236" height="42" rx="8" fill="var(--card)" stroke="var(--stroke)"/>
    <text x="388" y="66" text-anchor="middle" class="sans" font-size="12.5" font-weight="600" fill="var(--fg)">127.0.0.1 4-Col Web Inspector</text>

    <rect x="520" y="40" width="236" height="42" rx="8" fill="var(--card)" stroke="var(--stroke)"/>
    <text x="638" y="66" text-anchor="middle" class="sans" font-size="12.5" font-weight="600" fill="var(--fg)">Markdown export / apply / why</text>

    <rect x="770" y="40" width="234" height="42" rx="8" fill="var(--card)" stroke="var(--stroke)"/>
    <text x="887" y="66" text-anchor="middle" class="sans" font-size="12.5" font-weight="600" fill="var(--fg)">Community REST · MCP · A2A</text>
  </g>
</svg>"""


def video_poster_svg() -> str:
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1080 220" width="100%">
  <style>
    :root {{
      --bg: #0F172A;
      --card: #1E293B;
      --stroke: #334155;
      --fg: #F8FAFC;
      --muted: #94A3B8;
      --accent: #38BDF8;
      --green: #34D399;
    }}
    .sans {{ font-family: {FONT_SANS}; }}
    .mono {{ font-family: {FONT_MONO}; }}
  </style>
  <rect width="1080" height="220" rx="16" fill="var(--bg)" stroke="var(--stroke)" stroke-width="1.5"/>

  <g transform="translate(42, 46)">
    <circle cx="64" cy="64" r="52" fill="#0F2942" stroke="var(--accent)" stroke-width="2.5"/>
    <polygon points="52,40 52,88 92,64" fill="var(--accent)"/>
  </g>

  <g transform="translate(196, 44)">
    <rect x="0" y="0" width="178" height="26" rx="6" fill="var(--card)" stroke="var(--stroke)"/>
    <text x="89" y="17" text-anchor="middle" class="mono" font-size="11.5" font-weight="700" fill="var(--green)">INTERACTIVE TOUR · 5 MIN</text>

    <text x="0" y="60" class="sans" font-size="24" font-weight="800" fill="var(--fg)">See every subsystem execute live with zero API keys</text>
    <text x="0" y="90" class="sans" font-size="14.5" fill="var(--muted)">
      Run kp demo to exercise Claude Code and Codex hooks, semiring StalePremise invalidation, PreToolUse command
    </text>
    <text x="0" y="112" class="sans" font-size="14.5" fill="var(--muted)">
      guards, lossless Markdown write-back, Carol non-interference isolation, and the 127.0.0.1 Web Inspector.
    </text>
    <text x="0" y="142" class="mono" font-size="13" fill="var(--accent)">$ kp demo --dir ./kipo-demo &amp;&amp; kp inspect --path ./kipo-demo/demo-lib --as alice</text>
  </g>
</svg>"""


def social_preview_svg() -> str:
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1280 640" width="1280" height="640">
  <style>
    .sans {{ font-family: {FONT_SANS}; }}
    .mono {{ font-family: {FONT_MONO}; }}
  </style>
  <rect width="1280" height="640" fill="#0B111B"/>
  <rect x="48" y="48" width="1184" height="544" rx="28" fill="#111A29" stroke="#24344D" stroke-width="2"/>

  <g transform="translate(96, 104)">
    <rect width="84" height="84" rx="20" fill="#0F2942" stroke="#38BDF8" stroke-width="2.5"/>
    <circle cx="28" cy="28" r="8" fill="#38BDF8"/>
    <circle cx="58" cy="26" r="7" fill="#34D399"/>
    <circle cx="43" cy="58" r="8" fill="#FBBF24"/>
    <line x1="28" y1="28" x2="58" y2="26" stroke="#38BDF8" stroke-width="2.5"/>
    <line x1="28" y1="28" x2="43" y2="58" stroke="#38BDF8" stroke-width="2.5"/>
    <line x1="58" y1="26" x2="43" y2="58" stroke="#34D399" stroke-width="2.5"/>

    <text x="112" y="52" class="sans" font-size="54" font-weight="800" fill="#F8FAFC">Kipo</text>
    <rect x="248" y="14" width="76" height="46" rx="10" fill="#17253B" stroke="#2E4466"/>
    <text x="286" y="45" text-anchor="middle" class="mono" font-size="26" font-weight="700" fill="#38BDF8">kp</text>

    <text x="0" y="146" class="sans" font-size="30" font-weight="700" fill="#E2E8F0">
      Local-first personal and team memory for AI coding agents.
    </text>
    <text x="0" y="192" class="sans" font-size="20" fill="#94A3B8">
      One SQLite file · Read-time provenance semiring · Non-blocking hooks · Sleep-time skill evolution
    </text>

    <g transform="translate(0, 248)">
      <rect x="0" y="0" width="340" height="148" rx="16" fill="#162235" stroke="#293C59"/>
      <text x="28" y="42" class="mono" font-size="16" font-weight="700" fill="#38BDF8">SINGLE-FILE ENGINE</text>
      <text x="28" y="78" class="sans" font-size="20" font-weight="700" fill="#F8FAFC">Isomorphic kp.sqlite</text>
      <text x="28" y="110" class="sans" font-size="16" fill="#94A3B8">Redacted L0 blobs, CJK FTS5,</text>
      <text x="28" y="132" class="sans" font-size="16" fill="#94A3B8">local ONNX, bitemporal L1</text>

      <rect x="374" y="0" width="340" height="148" rx="16" fill="#162235" stroke="#293C59"/>
      <text x="402" y="42" class="mono" font-size="16" font-weight="700" fill="#34D399">ZERO COPIED ACLS</text>
      <text x="402" y="78" class="sans" font-size="20" font-weight="700" fill="#F8FAFC">Provenance semiring</text>
      <text x="402" y="110" class="sans" font-size="16" fill="#94A3B8">Instant forget, expiry, and</text>
      <text x="402" y="132" class="sans" font-size="16" fill="#94A3B8">roster contraction at read time</text>

      <rect x="748" y="0" width="340" height="148" rx="16" fill="#162235" stroke="#293C59"/>
      <text x="776" y="42" class="mono" font-size="16" font-weight="700" fill="#FBBF24">AGENT-NATIVE</text>
      <text x="776" y="78" class="sans" font-size="20" font-weight="700" fill="#F8FAFC">Claude Code &amp; Codex</text>
      <text x="776" y="110" class="sans" font-size="16" fill="#94A3B8">&lt;=250-tok recall, PreToolUse</text>
      <text x="776" y="132" class="sans" font-size="16" fill="#94A3B8">guards, MCP, kp dream skills</text>
    </g>
  </g>
</svg>"""


def main() -> None:
    write_svg(ASSETS / "kipo-hero-dark.svg", hero_svg(dark=True))
    write_svg(ASSETS / "kipo-hero-light.svg", hero_svg(dark=False))
    write_svg(ASSETS / "kipo-terminal-demo.svg", terminal_demo_svg())
    write_svg(ASSETS / "kipo-semiring.svg", semiring_svg())
    write_svg(ASSETS / "kipo-architecture.svg", architecture_svg())
    write_svg(ASSETS / "kipo-video-poster.svg", video_poster_svg())
    write_svg(ASSETS / "kipo-social-preview.svg", social_preview_svg())
    print(f"Generated 7 verified SVG assets in {ASSETS}")


if __name__ == "__main__":
    main()
