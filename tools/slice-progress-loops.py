#!/usr/bin/env python3
"""Slice registered whole-zone Chasers into per-LED loops for animated progress bars.

QLC+ blends Functions sharing a channel by per-channel maximum, so a bar cannot lay one
LED's look over a zone-wide Chaser: each LED needs its own loop. This tool makes them
mechanically from looks authored in QLC+ (never from colors of its own):

  for every [[loops]] set in config/progress-animation.toml, and every fixture the
  set's `source` Chaser writes, one Chaser whose step k is a Scene holding exactly that
  fixture's values from the source's step k, with the source's timing; plus one Virtual
  Console Cue List per loop, and per [[cues]] function, so the gateway can start a
  Chaser at a chosen step.

It rewrites the generated parts of that file and replaces everything it generated before
in the production workspace (Functions named "Loop ...", Cue Lists captioned "Loop cue"
or "Ambient cue"). Run it again after editing a source look in QLC+; `monolithd
validate-registry` reports loops that no longer match their source. Restart the lighting
stack afterwards so QLC+ loads the new workspace.

usage: slice-progress-loops.py [CONFIG_DIR]   (default: the repository's config/)
"""
import pathlib
import re
import sys
import tomllib

CHASER_BASE, SCENE_BASE, AMBIENT_CUE_BASE, LOOP_CUE_BASE = 2000, 20000, 9000, 10000

config = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else pathlib.Path(__file__).resolve().parent.parent / "config"
spec_path = config / "progress-animation.toml"
spec = tomllib.loads(spec_path.read_text())
registry = tomllib.loads((config / "qlc-functions.toml").read_text())
functions = {entry["name"]: entry for entry in registry["functions"]}
workspace_path = (config / registry["workspace"]).resolve()
xml = workspace_path.read_text()


def fail(message):
    sys.exit(f"slice-progress-loops: {message}")


# ---- remove what an earlier run generated
generated_function = re.compile(r'  <Function ID="(\d+)" Type="(?:Scene|Chaser)" Name="Loop [^"]*">.*?</Function>\n', re.S)
removed = [int(m.group(1)) for m in generated_function.finditer(xml)]
if any(fid < CHASER_BASE for fid in removed):
    fail(f"a Function below {CHASER_BASE} is named like a generated loop; rename it first")
xml = generated_function.sub("", xml)
xml, removed_cues = re.subn(r'   <CueList Caption="(?:Loop|Ambient) cue [^"]*" ID="\d+">.*?</CueList>\n', "", xml, flags=re.S)


def block(fid):
    m = re.search(r'  <Function ID="%d" .*?</Function>\n' % fid, xml, re.S)
    if not m:
        fail(f"Function {fid} is not in {workspace_path}")
    return m.group(0)


taken_functions = {int(i) for i in re.findall(r'<Function ID="(\d+)"', xml)}
taken_widgets = {int(i) for i in re.findall(r'<(?:Frame|SoloFrame|Button|Slider|Knob|CueList|Label|XYPad|SpeedDial|AudioTriggers|Clock|Matrix|Animation)\b[^>]*\bID="(\d+)"', xml)}


def claim(fid, taken, what):
    if fid in taken:
        fail(f"{what} ID {fid} is already used in the workspace")
    taken.add(fid)
    return fid


def cue_list(cid, caption, chaser):
    return (
        f'   <CueList Caption="{caption}" ID="{cid}">\n    <WindowState Visible="False" X="0" Y="0" Width="10" Height="10"/>\n'
        '    <Appearance>\n     <FrameStyle>None</FrameStyle>\n     <ForegroundColor>Default</ForegroundColor>\n     <BackgroundColor>Default</BackgroundColor>\n'
        f'     <BackgroundImage>None</BackgroundImage>\n     <Font>Default</Font>\n    </Appearance>\n    <Chaser>{chaser}</Chaser>\n   </CueList>\n'
    )


functions_xml, cues_xml, out_cues, out_loops = "", "", [], []
for j, entry in enumerate(spec.get("cues", [])):
    target = functions.get(entry["function"])
    if not target or target["kind"] != "chaser":
        fail(f"cue for {entry['function']}: not a registered chaser")
    cid = claim(AMBIENT_CUE_BASE + j, taken_widgets, "Cue List")
    cues_xml += cue_list(cid, f"Ambient cue {entry['function']}", target["id"])
    out_cues.append((entry["function"], cid))

chaser_id, scene_id, cue_id = CHASER_BASE, SCENE_BASE, LOOP_CUE_BASE
for loops in spec.get("loops", []):
    label = f"{loops['zone']} {loops['role']} {loops['look']}"
    source = functions.get(loops["source"])
    if not source or source["kind"] != "chaser" or source["zones"] != [loops["zone"]]:
        fail(f"loops {label}: source {loops['source']} must be a registered chaser on zone {loops['zone']} alone")
    head = block(source["id"])
    speed = re.search(r"<Speed [^>]*/>", head).group(0)
    modes = re.search(r"<SpeedModes [^>]*/>", head)
    step_ms = int(re.search(r'Duration="(\d+)"', speed).group(1))
    steps = [dict(re.findall(r'<FixtureVal ID="(\d+)">([^<]*)</FixtureVal>', block(child))) for child in source["children"]]
    fixtures = sorted(steps[0], key=int)
    if any(sorted(step, key=int) != fixtures for step in steps):
        fail(f"loops {label}: the steps of {loops['source']} write different fixtures")
    out = {**{key: loops[key] for key in ("zone", "role", "look", "source")}, "step_ms": step_ms, "steps": len(steps), "fixtures": [int(f) for f in fixtures], "chasers": [], "cue_lists": []}
    for fixture in fixtures:
        step_ids = []
        for k, step in enumerate(steps):
            sid = claim(scene_id, taken_functions, "Function")
            scene_id += 1
            functions_xml += (
                f'  <Function ID="{sid}" Type="Scene" Name="Loop {label} {fixture} {k + 1}">\n   <Speed FadeIn="0" FadeOut="0" Duration="0"/>\n'
                f'   <FixtureVal ID="{fixture}">{step[fixture]}</FixtureVal>\n  </Function>\n'
            )
            step_ids.append(sid)
        cid = claim(chaser_id, taken_functions, "Function")
        chaser_id += 1
        lines = [f'  <Function ID="{cid}" Type="Chaser" Name="Loop {label} {fixture}">', f"   {speed}", "   <Direction>Forward</Direction>", "   <RunOrder>Loop</RunOrder>"]
        if modes:
            lines.append(f"   {modes.group(0)}")
        lines += [f'   <Step Number="{n}" FadeIn="0" Hold="0" FadeOut="0">{s}</Step>' for n, s in enumerate(step_ids)]
        functions_xml += "\n".join(lines + ["  </Function>"]) + "\n"
        wid = claim(cue_id, taken_widgets, "Cue List")
        cue_id += 1
        cues_xml += cue_list(wid, f"Loop cue {cid}", cid)
        out["chasers"].append(cid)
        out["cue_lists"].append(wid)
    out_loops.append(out)

end = xml.rindex("</Function>") + len("</Function>\n")
xml = xml[:end] + functions_xml + xml[end:]
page = xml.index('<Frame Caption="Page 1" ID="0">')
close = xml.index("\n  </Frame>", page) + 1
xml = xml[:close] + cues_xml + xml[close:]
workspace_path.write_text(xml)


def ints(values):
    return "[" + ", ".join(map(str, values)) + "]"


text = [
    "# Animated progress bars: per-LED loops sliced from registered whole-zone Chasers.",
    "# Generated by tools/slice-progress-loops.py. Edit only each set's zone, role, look and",
    "# source (and the cues' functions), then re-run it; edit the looks themselves in QLC+.",
    "# role: full (the lit side) or empty; look: the ambient set the bar is drawn over, or",
    "# \"working\" for the distinct working look on the empty side.",
    "version = 1",
]
for function, cid in out_cues:
    text += ["", "[[cues]]", f'function = "{function}"', f"cue_list = {cid}"]
for out in out_loops:
    text += ["", "[[loops]]"] + [f'{key} = "{out[key]}"' for key in ("zone", "role", "look", "source")]
    text += [f"step_ms = {out['step_ms']}", f"steps = {out['steps']}"] + [f"{key} = {ints(out[key])}" for key in ("fixtures", "chasers", "cue_lists")]
spec_path.write_text("\n".join(text) + "\n")
print(f"removed {len(removed)} Functions and {removed_cues} Cue Lists from an earlier run")
print(f"wrote {chaser_id - CHASER_BASE} loops ({scene_id - SCENE_BASE} step Scenes), {len(out_cues)} ambient and {cue_id - LOOP_CUE_BASE} loop Cue Lists to {workspace_path.name}")
