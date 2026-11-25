#!/usr/bin/env python3
"""Verify open-source release readiness across docs, SVG assets, workflows, and privacy invariants."""

from pathlib import Path
import re
import sys
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parent.parent

REQUIRED_FILES = [
    "README.md",
    "README.zh-CN.md",
    "TUTORIAL.md",
    "LICENSE",
    "THIRD_PARTY_NOTICES.md",
    "CONTRIBUTING.md",
    "SECURITY.md",
    "CODE_OF_CONDUCT.md",
    "CHANGELOG.md",
    "SUPPORT.md",
    "docs/ARCHITECTURE.md",
    "docs/INGEST-SCHEMA.md",
    "docs/assets/kipo-logo.svg",
    "docs/assets/kipo-hero-dark.svg",
    "docs/assets/kipo-hero-light.svg",
    "docs/assets/kipo-terminal-demo.svg",
    "docs/assets/kipo-semiring.svg",
    "docs/assets/kipo-architecture.svg",
    "docs/assets/kipo-video-poster.svg",
    "docs/assets/kipo-social-preview.svg",
    "docs/assets/kipo-explainer.mp4",
    "docs/assets/kipo-explainer-preview.gif",
    ".github/workflows/ci.yml",
    ".github/workflows/release.yml",
    ".github/PULL_REQUEST_TEMPLATE.md",
    ".github/dependabot.yml",
    ".github/CODEOWNERS",
    ".github/ISSUE_TEMPLATE/config.yml",
    ".github/ISSUE_TEMPLATE/bug_report.yml",
    ".github/ISSUE_TEMPLATE/feature_request.yml",
]

UNSLOP_ENGLISH_FILES = [
    "README.md",
    "CONTRIBUTING.md",
    "SECURITY.md",
    "SUPPORT.md",
    "CHANGELOG.md",
    ".github/PULL_REQUEST_TEMPLATE.md",
]

BANNED_WORDS_RE = re.compile(
    r"\b(additionally|crucial|delve|delves|delving|enhance|enhances|foster|fosters|"
    r"landscape|pivotal|showcase|showcases|testament|underscore|underscores|vibrant|"
    r"utilize|utilizes|leverage|leverages|boasts)\b",
    re.IGNORECASE,
)

PERSONAL_PATH_PATTERNS = [
    re.compile("/" + "Users/"),
    re.compile("/" + "home/"),
    re.compile(r"[A-Za-z]:\\" + "Users" + r"\\"),
    re.compile(r"[A-Za-z]:/" + "Users/"),
]


def github_slug(heading: str) -> str:
    clean = re.sub(r"`([^`]*)`", r"\1", heading.strip())
    clean = re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", clean)
    clean = clean.lower()
    clean = re.sub(r"[^\w\s\u4e00-\u9fff-]", "", clean)
    clean = re.sub(r"\s+", "-", clean.strip())
    return clean


def collect_headings(md_path: Path) -> set[str]:
    slugs: set[str] = set()
    in_fence = False
    for line in md_path.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped.startswith("```") or stripped.startswith("~~~"):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        m = re.match(r"^#{1,6}\s+(.+?)\s*$", line)
        if m:
            slugs.add(github_slug(m.group(1)))
    return slugs


def strip_fenced_code(md_text: str) -> str:
    out = []
    in_fence = False
    for line in md_text.splitlines():
        s = line.strip()
        if s.startswith("```") or s.startswith("~~~"):
            in_fence = not in_fence
            continue
        if not in_fence:
            out.append(line)
    return "\n".join(out)


def check_markdown_links(md_path: Path, errors: list[str]) -> None:
    prose = strip_fenced_code(md_path.read_text(encoding="utf-8"))
    targets: list[str] = []
    for m in re.finditer(r"!?\[[^\]]*\]\(([^)]+)\)", prose):
        raw = m.group(1).strip().split()[0].strip("<>")
        targets.append(raw)
    for m in re.finditer(r'\b(?:src|srcset|href)="([^"]+)"', prose):
        targets.append(m.group(1).strip())

    for target in targets:
        if not target or target.startswith(("http://", "https://", "mailto:")):
            continue
        path_part, _, frag = target.partition("#")
        resolved = (md_path.parent / path_part).resolve() if path_part else md_path.resolve()
        if not resolved.exists():
            errors.append(f"{md_path.relative_to(ROOT)}: broken relative link '{target}'")
            continue
        if frag and resolved.suffix.lower() == ".md" and not frag.startswith("gh-"):
            headings = collect_headings(resolved)
            if frag.lower() not in headings:
                errors.append(
                    f"{md_path.relative_to(ROOT)}: missing anchor '#{frag}' in {resolved.relative_to(ROOT)}"
                )


def check_svg_files(errors: list[str]) -> None:
    for svg_path in sorted((ROOT / "docs" / "assets").glob("*.svg")):
        raw = svg_path.read_text(encoding="utf-8")
        try:
            root = ET.fromstring(raw)
        except ET.ParseError as exc:
            errors.append(f"{svg_path.relative_to(ROOT)}: XML parse error: {exc}")
            continue
        if not root.tag.endswith("svg"):
            errors.append(f"{svg_path.relative_to(ROOT)}: root tag is not <svg>")
        if "viewBox" not in root.attrib:
            errors.append(f"{svg_path.relative_to(ROOT)}: missing viewBox")
        if "<script" in raw.lower():
            errors.append(f"{svg_path.relative_to(ROOT)}: <script> forbidden in GitHub README SVGs")
        if re.search(r"https?://(?!www\.w3\.org/)", raw):
            errors.append(f"{svg_path.relative_to(ROOT)}: external URL forbidden in standalone SVG")


def check_workflows_and_templates(errors: list[str]) -> None:
    for yml in sorted((ROOT / ".github").rglob("*.yml")):
        text = yml.read_text(encoding="utf-8")
        for lineno, line in enumerate(text.splitlines(), 1):
            if "\t" in line:
                errors.append(f"{yml.relative_to(ROOT)}:{lineno}: tab character in YAML")
    release_text = (ROOT / ".github" / "workflows" / "release.yml").read_text(encoding="utf-8")
    if "attest-build-provenance" not in release_text:
        errors.append(".github/workflows/release.yml: missing actions/attest-build-provenance step")
    if "inputs.tag || github.ref" not in release_text:
        errors.append(".github/workflows/release.yml: checkout missing workflow_dispatch tag ref")


def check_unslop_and_privacy(errors: list[str]) -> None:
    for rel in UNSLOP_ENGLISH_FILES:
        path = ROOT / rel
        if not path.exists():
            continue
        prose = strip_fenced_code(path.read_text(encoding="utf-8"))
        prose = re.sub(r"\]\([^)]+\)", "]()", prose)
        prose = re.sub(r'\b(?:href|src|srcset)="[^"]+"', "", prose)
        for lineno, line in enumerate(prose.splitlines(), 1):
            m = BANNED_WORDS_RE.search(line)
            if m:
                errors.append(f"{rel}:{lineno}: banned AI vocabulary '{m.group(0)}'")
            if "\u2014" in line or "\u2013" in line:
                errors.append(f"{rel}:{lineno}: em/en dash found in prose")

    for path in sorted(ROOT.rglob("*")):
        if not path.is_file():
            continue
        rel_parts = path.relative_to(ROOT).parts
        if rel_parts[0] in {".git", "target"}:
            continue
        if path.suffix.lower() not in {".md", ".yml", ".yaml", ".toml", ".sh", ".ps1", ".py", ".svg", ".rs"}:
            continue
        content = path.read_text(encoding="utf-8", errors="ignore")
        for pat in PERSONAL_PATH_PATTERNS:
            if pat.search(content):
                errors.append(f"{path.relative_to(ROOT)}: personal home path prefix matched '{pat.pattern}'")


def main() -> int:
    errors: list[str] = []
    for rel in REQUIRED_FILES:
        p = ROOT / rel
        if not p.is_file() or p.stat().st_size == 0:
            errors.append(f"missing or empty required file: {rel}")

    if not errors:
        for md in [
            ROOT / "README.md",
            ROOT / "README.zh-CN.md",
            ROOT / "TUTORIAL.md",
            ROOT / "CONTRIBUTING.md",
            ROOT / "SECURITY.md",
            ROOT / "SUPPORT.md",
            ROOT / "CHANGELOG.md",
            ROOT / "THIRD_PARTY_NOTICES.md",
            ROOT / "docs" / "ARCHITECTURE.md",
            ROOT / "docs" / "INGEST-SCHEMA.md",
            ROOT / "examples" / "showcase" / "README.md",
        ]:
            check_markdown_links(md, errors)
        check_svg_files(errors)
        check_workflows_and_templates(errors)
        check_unslop_and_privacy(errors)

    if errors:
        for err in errors:
            print(f"FAIL: {err}", file=sys.stderr)
        return 1

    print(
        f"OK: verified {len(REQUIRED_FILES)} OSS files, Markdown links/anchors, SVG schemas, YAML workflows, and prose/privacy gates."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
