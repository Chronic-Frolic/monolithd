"""Add warning_ram/warning_strip/working_ram/working_strip: bootstrap content for the
newly whole-machine-capable warning/working states, per owner request 2026-09-22.
Seeded from the already-accepted eye color (amber for warning, white for working),
replicated across RAM/Strip fixtures -- not a new color decision, an existing
accepted one extended to more zones as a starting point the owner repaints via the
intake workflow, same bootstrap principle as the ambient/fault template."""
import re

QXW = "qlcplus/monolith-lighting.qxw"
REGISTRY = "monolith-events/qlc-functions.toml"

text = open(QXW).read()

WARNING_RGB = (255, 176, 0)  # matches warning_eye / rgb-palette.toml's warning color
WORKING_RGB = (255, 255, 255)  # matches working_eye / reference_white_eye


def ram_scene(function_id: int, name: str, rgb: tuple[int, int, int]) -> str:
    r, g, b = rgb
    lines = [f'  <Function ID="{function_id}" Type="Scene" Name="{name}">', '   <Speed FadeIn="0" FadeOut="0" Duration="0"/>']
    for fixture in range(32):
        lines.append(f'   <FixtureVal ID="{fixture}">0,{r},1,{g},2,{b}</FixtureVal>')
    lines.append("  </Function>")
    return "\n".join(lines)


def strip_scene(function_id: int, name: str, rgb: tuple[int, int, int]) -> str:
    r, g, b = rgb
    parts = []
    ch = 0
    for _ in range(70):
        parts += [f"{ch},{r}", f"{ch+1},{g}", f"{ch+2},{b}"]
        ch += 3
    return f'  <Function ID="{function_id}" Type="Scene" Name="{name}">\n   <Speed FadeIn="0" FadeOut="0" Duration="0"/>\n   <FixtureVal ID="41">{",".join(parts)}</FixtureVal>\n  </Function>'


new_functions = [
    ram_scene(200, "Warning RAM", WARNING_RGB),
    strip_scene(201, "Warning Strip", WARNING_RGB),
    ram_scene(202, "Working RAM", WORKING_RGB),
    strip_scene(203, "Working Strip", WORKING_RGB),
]

marker = '  <Function ID="127" Type="Scene" Name="Working Eye">\n   <Speed FadeIn="0" FadeOut="0" Duration="0"/>\n   <FixtureVal ID="40">0,255,1,255,2,255,3,255,4,255,5,255,6,255,7,255,8,255</FixtureVal>\n  </Function>'
assert text.count(marker) == 1, "Working Eye marker not found or not unique"
text = text.replace(marker, marker + "\n" + "\n".join(new_functions))
open(QXW, "w").write(text)
print(f"{QXW}: added {len(new_functions)} Functions (200-203)")

reg = open(REGISTRY).read()
marker2 = '''[[functions]]
name = "working_eye"
kind = "scene"
id = 127
children = []
zones = ["rog_eye"]
composable = true'''
addition = '''

[[functions]]
name = "warning_ram"
kind = "scene"
id = 200
children = []
zones = ["ram"]
composable = true

[[functions]]
name = "warning_strip"
kind = "scene"
id = 201
children = []
zones = ["strip"]
composable = true

[[functions]]
name = "working_ram"
kind = "scene"
id = 202
children = []
zones = ["ram"]
composable = true

[[functions]]
name = "working_strip"
kind = "scene"
id = 203
children = []
zones = ["strip"]
composable = true'''
assert reg.count(marker2) == 1, "working_eye registry entry not found or not unique"
reg = reg.replace(marker2, marker2 + addition)
open(REGISTRY, "w").write(reg)
print(f"{REGISTRY}: registered warning_ram/warning_strip/working_ram/working_strip")
