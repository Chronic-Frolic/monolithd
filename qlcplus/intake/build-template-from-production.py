"""Build the starter intake template by extracting today's live production content
and merging each contract entry's per-zone Functions back into one whole-machine
Function -- the exact inverse of what the splitter (workspace.rs) will do. Seeds the
owner's starting point with today's real, already-accepted look, not a blank file.

One-time bootstrap, not part of the runtime -- `monolithd workspace select` is the
tool that runs repeatedly. Rerun this only to regenerate the starter template from
whatever's live at the time (e.g. after a deliberate hand-authored change to it).

Run from the repo's `qlcplus/` directory's parent (~/monolithd):
    python3 qlcplus/intake/build-template-from-production.py
"""
import re

SRC = "qlcplus/monolith-lighting.qxw"
OUT = "qlcplus/intake/Monolithd Intake Template.qxw"

text = open(SRC).read()


def function_block(function_id: int) -> str:
    pattern = re.compile(rf'  <Function ID="{function_id}" .*?</Function>', re.S)
    m = pattern.search(text)
    assert m, f"Function {function_id} not found"
    return m.group(0)


def fixture_vals(block: str) -> list[tuple[int, list[tuple[int, int]]]]:
    out = []
    for fid, body in re.findall(r'<FixtureVal ID="(\d+)">([^<]*)</FixtureVal>', block):
        nums = [int(x) for x in body.split(",")]
        pairs = list(zip(nums[0::2], nums[1::2]))
        out.append((int(fid), pairs))
    return out


def render_fixture_val(fixture_id: int, triples: list[tuple[int, int, int]]) -> str:
    parts = []
    ch = 0
    for r, g, b in triples:
        parts += [f"{ch},{r}", f"{ch+1},{g}", f"{ch+2},{b}"]
        ch += 3
    return f'   <FixtureVal ID="{fixture_id}">{",".join(parts)}</FixtureVal>'


def triples_of(block: str) -> dict[int, tuple[int, int, int]]:
    out = {}
    for fid, pairs in fixture_vals(block):
        vals = [v for _, v in sorted(pairs)]
        assert len(vals) % 3 == 0
        # Keep every RGB triple under its fixture ID; current fixtures each have one LED.
        out[fid] = [tuple(vals[i:i+3]) for i in range(0, len(vals), 3)]
    return out


next_id = [0]


def fresh_id() -> int:
    next_id[0] += 1
    return next_id[0]


def merged_scene(fid: int, name: str, source_ids: list[int]) -> str:
    """Merge N single-zone Functions (fixture ids disjoint across the group) into
    one whole-machine Scene block, verbatim -- no color decided, just concatenated."""
    lines = [f'  <Function ID="{fid}" Type="Scene" Name="{name}">', '   <Speed FadeIn="0" FadeOut="0" Duration="0"/>']
    seen_fixtures = set()
    for source_id in source_ids:
        block = function_block(source_id)
        for fixture_id, triples in sorted(triples_of(block).items()):
            assert fixture_id not in seen_fixtures, f"fixture {fixture_id} claimed by more than one source for {name}"
            seen_fixtures.add(fixture_id)
            lines.append(render_fixture_val(fixture_id, triples))
    lines.append("  </Function>")
    return "\n".join(lines)


def merged_chaser(name: str, low_ids: list[int], high_ids: list[int], speed_from_id: int) -> str:
    speed_block = function_block(speed_from_id)
    speed_tag = re.search(r'<Speed [^/]*/>', speed_block).group(0)
    low_id, high_id, chaser_id = fresh_id(), fresh_id(), fresh_id()
    low = merged_scene(low_id, f"{name} — Low", low_ids)
    high = merged_scene(high_id, f"{name} — High", high_ids)
    chaser = (
        f'  <Function ID="{chaser_id}" Type="Chaser" Name="{name}">\n'
        f'   {speed_tag}\n'
        '   <Direction>Forward</Direction>\n'
        '   <RunOrder>Loop</RunOrder>\n'
        f'   <Step Number="0">{low_id}</Step>\n'
        f'   <Step Number="1">{high_id}</Step>\n'
        '  </Function>'
    )
    return "\n".join([low, high, chaser])


def progress_keyframe(source_id: int, name: str) -> str:
    """Rename an existing progress-family step Scene into a named keyframe pair
    member, verbatim -- no color decided, just relabeled with a fresh ID."""
    block = function_block(source_id)
    new_fid = fresh_id()
    block = re.sub(r'<Function ID="\d+"', f'<Function ID="{new_fid}"', block, count=1)
    block = re.sub(r'Name="[^"]*"', f'Name="{name}"', block, count=1)
    return block


functions = [
    merged_chaser("Base Ambient", low_ids=[107, 110, 113], high_ids=[108, 111, 114], speed_from_id=109),
    merged_scene(fresh_id(), "State — Fault", [118, 119, 126]),
    merged_scene(fresh_id(), "State — Warning", [200, 117, 201]),
    merged_scene(fresh_id(), "State — Working", [202, 127, 203]),
    merged_scene(fresh_id(), "State — Controller Fault", [123, 124, 125]),
    # Progress keyframes: step 0 (all-empty) and the final step (all-full) of each
    # existing progress family, relabeled -- the same real flat white/green content
    # already in production today, not a new decision. progress_ram_interleaved
    # shares the RAM pair (see workspace.rs's progress_keyframe_names): it differs
    # only in fill order, which is measured, not authored per-family.
    progress_keyframe(0, "Progress RAM — Empty"),
    progress_keyframe(32, "Progress RAM — Full"),
    progress_keyframe(33, "Progress Strip — Empty"),
    progress_keyframe(103, "Progress Strip — Full"),
]

header = text[: text.index('  <Function ID="0"')]  # Creator + Engine open + InputOutputMap + Fixtures + early FixtureGroups
# The header stops at the first Function, but production also keeps content after it that the
# intake files must carry: fixture groups saved later (e.g. "RAM", ID 38) and the 2D <Monitor>
# layout. Dropping them leaves QLC's Fixtures & Functions 2D view with no saved positions and
# breaks RGB Matrix intake (the importer compares group blocks to production verbatim).
later_groups = "".join(
    m.group(0)
    for m in re.finditer(r'[ \t]*<FixtureGroup ID="(\d+)">.*?</FixtureGroup>\n', text[len(header):], re.S)
)
monitor = re.search(r'[ \t]*<Monitor [^>]*>.*?</Monitor>\n', text, re.S)
assert monitor, "production has no <Monitor> layout to carry over"

footer = "\n" + monitor.group(0) + " </Engine>\n</Workspace>\n"

out = header + later_groups + "\n".join(functions) + "\n" + footer

import os
os.makedirs("qlcplus/intake", exist_ok=True)
open(OUT, "w").write(out)
print(f"wrote {OUT}, {len(functions)} contract Functions")
for f in functions:
    for m in re.finditer(r'Name="([^"]+)"', f):
        print(" -", m.group(1))
