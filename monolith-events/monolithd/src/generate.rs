//! Regenerate the palette-driven state Scenes -- the two-tone progress families and the
//! flat-color warning/fault/controller-fault/working scenes -- from `rgb-palette.toml`,
//! deterministically. Replaces the one-off hand-scripts this work used to need (e.g.
//! `gen_interleaved_ram.py`) with a durable, rerunnable subcommand: `monolithd
//! generate-state-scenes`.
//!
//! Design constraint, load-bearing: scenes are authored in **nominal** color.
//! Hardware correction lives only in `led-calibration.toml`, applied at the output
//! stage (`renderer.rs`'s `QlcOutput::apply`). This tool must never pre-correct for a
//! zone's calibration gain -- doing so would double-correct once the calibrated
//! output stage also applies its gain.
//!
//! The progress families' fill order (which physical position lights up at which
//! step) is never hand-encoded here. It is measured by diffing the family's own
//! already-authored steps against each other, so a change to the physical fill
//! geometry never needs a matching change in this file.
//!
//! `reference_white_eye` and the `quiet_*` scenes are deliberately never touched: the
//! former is the calibration instrument (must stay a fixed, known white to be useful
//! as a comparison point), the latter is "off" by definition, not a palette color.

use crate::registry;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub struct Rgb(pub u8, pub u8, pub u8);

#[derive(Debug, serde::Deserialize)]
struct PaletteFile {
    colors: PaletteColors,
}

#[derive(Debug, serde::Deserialize)]
struct PaletteColors {
    primary: Rgb,
    secondary: Rgb,
    warning: Rgb,
    fault: Rgb,
    controller_failure: Rgb,
    #[serde(default = "default_working")]
    working: Rgb,
}

fn default_working() -> Rgb {
    Rgb(255, 255, 255)
}

/// Which flat-color scenes exist and which palette role each one takes. Extend this
/// table, not the palette file's shape, when a new flat-color state scene is authored;
/// `reference_white_eye` and `quiet_*` are excluded on purpose, see the module doc.
const FLAT_SCENES: &[(&str, fn(&PaletteColors) -> Rgb)] = &[
    ("warning_eye", |p| p.warning),
    ("fault_ram", |p| p.fault),
    ("fault_eye", |p| p.fault),
    ("fault_strip", |p| p.fault),
    ("controller_fault_ram", |p| p.controller_failure),
    ("controller_fault_eye", |p| p.controller_failure),
    ("controller_fault_strip", |p| p.controller_failure),
    ("working_eye", |p| p.working),
];

fn load_palette(path: &Path) -> Result<PaletteColors, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let file: PaletteFile = toml::from_str(&text).map_err(|error| format!("parse {}: {error}", path.display()))?;
    Ok(file.colors)
}

/// One `<FixtureVal ID="...">...</FixtureVal>` tag: its exact original text (for a
/// safe single-occurrence replace) and its decoded RGB triples in channel order.
struct FixtureVal {
    original: String,
    fixture_id: u32,
    triples: Vec<Rgb>,
}

/// The exact text of `<Function ID="id" ...>...</Function>` in `xml`. Scene Functions
/// (the only kind this tool touches) never nest, so a plain substring search to the
/// next `</Function>` is safe.
fn function_block<'a>(xml: &'a str, id: u32) -> Result<&'a str, String> {
    let open = format!("<Function ID=\"{id}\" ");
    let start = xml.find(&open).ok_or_else(|| format!("Function {id} not found in the workspace"))?;
    let close = "</Function>";
    let end = xml[start..].find(close).ok_or_else(|| format!("Function {id}: no closing tag"))? + start + close.len();
    Ok(&xml[start..end])
}

fn parse_fixture_vals(block: &str) -> Result<Vec<FixtureVal>, String> {
    let mut out = Vec::new();
    let mut rest = block;
    while let Some(tag_start) = rest.find("<FixtureVal ID=\"") {
        let tail = &rest[tag_start..];
        let end = tail.find("</FixtureVal>").ok_or("unterminated FixtureVal")? + "</FixtureVal>".len();
        let original = tail[..end].to_owned();

        let id_start = "<FixtureVal ID=\"".len();
        let id_end = tail[id_start..].find('"').ok_or("malformed FixtureVal ID")? + id_start;
        let fixture_id: u32 = tail[id_start..id_end].parse().map_err(|_| "non-numeric FixtureVal ID")?;

        let body_start = tail[id_end..].find('>').ok_or("malformed FixtureVal tag")? + id_end + 1;
        let body_end = tail.find("</FixtureVal>").unwrap();
        let body = &tail[body_start..body_end];

        let numbers: Vec<u32> = body.split(',').map(|token| token.trim().parse().map_err(|_| format!("bad channel/value token {token:?}"))).collect::<Result<_, String>>()?;
        if numbers.len() % 2 != 0 {
            return Err(format!("FixtureVal {fixture_id} has an odd channel/value count"));
        }
        let values: Vec<u8> = numbers.chunks(2).map(|pair| pair[1] as u8).collect();
        if values.len() % 3 != 0 {
            return Err(format!("FixtureVal {fixture_id} has {} channels, not a multiple of three", values.len()));
        }
        let triples = values.chunks(3).map(|c| Rgb(c[0], c[1], c[2])).collect();

        out.push(FixtureVal { original, fixture_id, triples });
        rest = &tail[end..];
    }
    if out.is_empty() {
        return Err("no FixtureVal tags found".to_owned());
    }
    Ok(out)
}

fn render_fixture_val(fixture_id: u32, triples: &[Rgb]) -> String {
    let mut channel = 0u32;
    let mut parts = Vec::with_capacity(triples.len() * 3);
    for Rgb(r, g, b) in triples {
        parts.push(format!("{channel},{r}"));
        channel += 1;
        parts.push(format!("{channel},{g}"));
        channel += 1;
        parts.push(format!("{channel},{b}"));
        channel += 1;
    }
    format!("<FixtureVal ID=\"{fixture_id}\">{}</FixtureVal>", parts.join(","))
}

/// Replace every FixtureVal in Function `id` with `color`, preserving the fixture IDs
/// and per-fixture channel counts exactly as authored -- this tool never assumes a
/// zone's shape, it reads it.
fn rewrite_flat(xml: &str, id: u32, color: Rgb) -> Result<String, String> {
    let block = function_block(xml, id)?.to_owned();
    let fixture_vals = parse_fixture_vals(&block)?;
    let mut new_block = block.clone();
    for fv in &fixture_vals {
        let replacement = vec![color; fv.triples.len()];
        let new_tag = render_fixture_val(fv.fixture_id, &replacement);
        if new_block.matches(fv.original.as_str()).count() != 1 {
            return Err(format!("Function {id}: FixtureVal {} is not uniquely located; refusing to guess", fv.fixture_id));
        }
        new_block = new_block.replacen(fv.original.as_str(), &new_tag, 1);
    }
    if xml.matches(block.as_str()).count() != 1 {
        return Err(format!("Function {id}: block is not uniquely located in the workspace"));
    }
    Ok(xml.replacen(block.as_str(), &new_block, 1))
}

/// A single fill position: which FixtureVal (by fixture ID) and which triple within
/// it. Positions are ordered by the step at which they first turn "complete".
type Position = (u32, usize);

/// Derive the family's fill order by diffing each step's FixtureVal triples against
/// the previous step's -- measured from the already-authored, physically-verified
/// steps, never hand-encoded. Exactly one position must newly differ per step, since
/// a "Progress X of N" family is authored as N discrete single-position increments;
/// anything else means the family isn't shaped the way this tool assumes, and it
/// refuses rather than guessing.
fn derive_fill_order(xml: &str, first_id: u32, total: u32) -> Result<Vec<Position>, String> {
    let mut previous = parse_fixture_vals(function_block(xml, first_id)?)?;
    let mut order = Vec::new();
    for step in 1..=total {
        let current = parse_fixture_vals(function_block(xml, first_id + step)?)?;
        if current.len() != previous.len() {
            return Err(format!("step {step}: fixture count changed ({} vs {})", current.len(), previous.len()));
        }
        let mut changed = Vec::new();
        for (prev_fv, cur_fv) in previous.iter().zip(current.iter()) {
            if prev_fv.fixture_id != cur_fv.fixture_id {
                return Err(format!("step {step}: fixture order changed"));
            }
            for (index, (prev_triple, cur_triple)) in prev_fv.triples.iter().zip(cur_fv.triples.iter()).enumerate() {
                if prev_triple != cur_triple {
                    changed.push((prev_fv.fixture_id, index));
                }
            }
        }
        if changed.len() != 1 {
            return Err(format!("step {step}: {} positions changed, expected exactly 1 -- this family isn't a simple one-position-per-step fill", changed.len()));
        }
        order.push(changed[0]);
        previous = current;
    }
    if order.len() != total as usize {
        return Err(format!("derived {} fill positions, expected {total}", order.len()));
    }
    Ok(order)
}

/// Regenerate every step (0..=total) of a progress family with new incomplete/complete
/// colors, using a fill order measured from the family's own current steps.
fn rewrite_progressive(xml: &str, first_id: u32, total: u32, incomplete: Rgb, complete: Rgb) -> Result<String, String> {
    let order = derive_fill_order(xml, first_id, total)?;
    let base = parse_fixture_vals(function_block(xml, first_id)?)?;
    let mut result = xml.to_owned();
    for step in 0..=total {
        let complete_positions: std::collections::HashSet<Position> = order[..step as usize].iter().copied().collect();
        let mut per_fixture: Vec<(u32, Vec<Rgb>)> = Vec::with_capacity(base.len());
        for fv in &base {
            let triples = (0..fv.triples.len())
                .map(|local_index| if complete_positions.contains(&(fv.fixture_id, local_index)) { complete } else { incomplete })
                .collect();
            per_fixture.push((fv.fixture_id, triples));
        }

        let id = first_id + step;
        let block = function_block(&result, id)?.to_owned();
        let current = parse_fixture_vals(&block)?;
        let mut new_block = block.clone();
        for (fv, (fixture_id, triples)) in current.iter().zip(per_fixture.iter()) {
            if fv.fixture_id != *fixture_id {
                return Err(format!("step {step}: fixture order drifted mid-rewrite"));
            }
            let new_tag = render_fixture_val(*fixture_id, triples);
            if new_block.matches(fv.original.as_str()).count() != 1 {
                return Err(format!("step {step}: FixtureVal {fixture_id} is not uniquely located; refusing to guess"));
            }
            new_block = new_block.replacen(fv.original.as_str(), &new_tag, 1);
        }
        if result.matches(block.as_str()).count() != 1 {
            return Err(format!("step {step} (Function {id}): block is not uniquely located"));
        }
        result = result.replacen(block.as_str(), &new_block, 1);
    }
    Ok(result)
}

/// `monolithd generate-state-scenes [--check]`. Reads `rgb-palette.toml` and
/// `qlc-functions.toml`, rewrites the workspace's progress and flat-color state
/// Scenes to match, and writes the result back. `--check` reports what would change
/// without writing, and is also the idempotency self-test: running it twice with an
/// unchanged palette must report no diff the second time.
pub fn run(arguments: Vec<String>) -> Result<(), String> {
    let check_only = arguments.iter().any(|a| a == "--check");
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    let registry_path = root.join("qlc-functions.toml");
    let palette_path = root.join("rgb-palette.toml");

    let registry = registry::load(&registry_path)?;
    let workspace_path = registry.workspace_path(&registry_path);
    let palette = load_palette(&palette_path)?;

    let original = std::fs::read_to_string(&workspace_path).map_err(|error| format!("read {}: {error}", workspace_path.display()))?;
    let mut xml = original.clone();
    let mut touched = Vec::new();

    for (name, color_of) in FLAT_SCENES {
        let entry = registry.function(name).ok_or_else(|| format!("{name} is not registered in qlc-functions.toml"))?;
        let color = color_of(&palette);
        xml = rewrite_flat(&xml, entry.id, color)?;
        touched.push(format!("{name} (Function {}) -> {color:?}", entry.id));
    }

    for progress in &registry.progress {
        xml = rewrite_progressive(&xml, progress.first_id, progress.total, palette.primary, palette.secondary)?;
        touched.push(format!(
            "{} (Functions {}-{}) -> incomplete {:?}, complete {:?}",
            progress.name,
            progress.first_id,
            progress.first_id + progress.total,
            palette.primary,
            palette.secondary
        ));
    }

    if xml == original {
        println!("no change: the workspace already matches rgb-palette.toml");
        return Ok(());
    }

    println!("{}:", if check_only { "would update" } else { "updating" });
    for line in &touched {
        println!("  {line}");
    }

    if check_only {
        return Ok(());
    }

    let backup_dir = std::path::PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?).join(".local/share/monolith-events/backups");
    std::fs::create_dir_all(&backup_dir).map_err(|error| format!("create {}: {error}", backup_dir.display()))?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|error| error.to_string())?.as_secs();
    let backup_path = backup_dir.join(format!("monolith-lighting.qxw.{stamp}.before-generate-state-scenes"));
    std::fs::write(&backup_path, &original).map_err(|error| format!("write backup {}: {error}", backup_path.display()))?;
    println!("backup: {}", backup_path.display());

    std::fs::write(&workspace_path, &xml).map_err(|error| format!("write {}: {error}", workspace_path.display()))?;
    println!("wrote {}", workspace_path.display());
    println!("next: `monolithd validate-registry`, then restart monolith-lighting-stack.service for QLC+ to reload it.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ram_family() -> String {
        // A tiny 3-position "Progress X of 3" family plus one flat scene, in the
        // same textual shape the real workspace uses.
        let mut xml = String::new();
        for step in 0..=3u32 {
            let complete_upto = step as usize;
            let mut fixtures = String::new();
            for fixture in 0..3u32 {
                let (r, g, b) = if (fixture as usize) < complete_upto { (0u8, 255u8, 0u8) } else { (255, 255, 255) };
                fixtures += &format!("   <FixtureVal ID=\"{fixture}\">0,{r},1,{g},2,{b}</FixtureVal>\n");
            }
            xml += &format!("  <Function ID=\"{step}\" Type=\"Scene\" Name=\"Progress Test {step:02} of 3\">\n   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"0\"/>\n{fixtures}  </Function>\n");
        }
        xml += "  <Function ID=\"200\" Type=\"Scene\" Name=\"Flat Test\">\n   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"0\"/>\n   <FixtureVal ID=\"40\">0,255,1,176,2,0,3,255,4,176,5,0</FixtureVal>\n  </Function>\n";
        xml
    }

    #[test]
    fn derives_the_sequential_fill_order_from_real_data() {
        let xml = sample_ram_family();
        let order = derive_fill_order(&xml, 0, 3).unwrap();
        assert_eq!(order, vec![(0, 0), (1, 0), (2, 0)]);
    }

    #[test]
    fn rewrites_a_progressive_family_preserving_the_measured_order() {
        let xml = sample_ram_family();
        let rewritten = rewrite_progressive(&xml, 0, 3, Rgb(50, 0, 255), Rgb(150, 0, 255)).unwrap();
        // Step 0: every position incomplete (deep purple).
        let step0 = function_block(&rewritten, 0).unwrap();
        assert!(step0.contains("<FixtureVal ID=\"0\">0,50,1,0,2,255</FixtureVal>"));
        assert!(step0.contains("<FixtureVal ID=\"2\">0,50,1,0,2,255</FixtureVal>"));
        // Step 2: positions 0 and 1 complete, 2 still incomplete.
        let step2 = function_block(&rewritten, 2).unwrap();
        assert!(step2.contains("<FixtureVal ID=\"0\">0,150,1,0,2,255</FixtureVal>"));
        assert!(step2.contains("<FixtureVal ID=\"1\">0,150,1,0,2,255</FixtureVal>"));
        assert!(step2.contains("<FixtureVal ID=\"2\">0,50,1,0,2,255</FixtureVal>"));
        // Step 3: fully complete.
        let step3 = function_block(&rewritten, 3).unwrap();
        assert!(step3.contains("<FixtureVal ID=\"2\">0,150,1,0,2,255</FixtureVal>"));
        // Unrelated Function 200 is untouched.
        assert!(rewritten.contains("<Function ID=\"200\" Type=\"Scene\" Name=\"Flat Test\">"));
        assert!(rewritten.contains("<FixtureVal ID=\"40\">0,255,1,176,2,0,3,255,4,176,5,0</FixtureVal>"));
    }

    #[test]
    fn rewriting_with_the_same_colors_already_present_is_idempotent() {
        let xml = sample_ram_family();
        let rewritten = rewrite_progressive(&xml, 0, 3, Rgb(255, 255, 255), Rgb(0, 255, 0)).unwrap();
        assert_eq!(rewritten, xml, "re-encoding the same colors must reproduce byte-identical XML");
    }

    #[test]
    fn rewrites_a_flat_scene_preserving_fixture_ids_and_count() {
        let xml = sample_ram_family();
        let rewritten = rewrite_flat(&xml, 200, Rgb(255, 0, 255)).unwrap();
        let block = function_block(&rewritten, 200).unwrap();
        assert!(block.contains("<FixtureVal ID=\"40\">0,255,1,0,2,255,3,255,4,0,5,255</FixtureVal>"));
        // Unrelated progress family untouched.
        assert!(rewritten.contains("<Function ID=\"0\" Type=\"Scene\" Name=\"Progress Test 00 of 3\">"));
    }

    #[test]
    fn a_family_that_is_not_a_simple_one_position_per_step_fill_is_refused() {
        // Two positions change between steps 0 and 1 -- not a shape this tool
        // understands, so it must refuse rather than guess an order.
        let mut xml = String::new();
        xml += "  <Function ID=\"0\" Type=\"Scene\" Name=\"Bad 00\">\n   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"0\"/>\n   <FixtureVal ID=\"0\">0,255,1,255,2,255</FixtureVal>\n   <FixtureVal ID=\"1\">0,255,1,255,2,255</FixtureVal>\n  </Function>\n";
        xml += "  <Function ID=\"1\" Type=\"Scene\" Name=\"Bad 01\">\n   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"0\"/>\n   <FixtureVal ID=\"0\">0,0,1,255,2,0</FixtureVal>\n   <FixtureVal ID=\"1\">0,0,1,255,2,0</FixtureVal>\n  </Function>\n";
        assert!(derive_fill_order(&xml, 0, 1).is_err());
    }
}
