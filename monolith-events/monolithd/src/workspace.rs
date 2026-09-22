//! Build the production QLC+ workspace from a whole-machine "intake" workspace the
//! owner authors directly in QLC+'s own GUI -- one cohesive Function per state, every
//! zone together, exactly the way the owner wants to author it. This module never
//! decides a color or a timing value: everything in its output is the owner's own
//! FixtureVal/Speed data from the intake file, filtered by zone. See "Named-function
//! intake contract" in the handoff note for why (QLC+'s API can only start/stop a
//! Function that already exists by ID in the loaded workspace -- it cannot create or
//! partially start one -- so independent per-zone control requires the per-zone
//! pieces to physically exist before the stack boots, and *something* has to produce
//! them from the owner's one combined Function).
//!
//! The owner-facing contract: an intake file may contain exactly the Functions named
//! in `CONTRACT` below, each written however the owner likes in QLC+'s GUI (color,
//! per-pixel art, chaser timing), but never renamed and never made to write fixtures
//! outside the zones that contract entry is allowed to touch. Everything else in the
//! intake file (reference scenes, works in progress, anything not in `CONTRACT`) is
//! ignored.

use crate::registry::{self, Registry};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One contract entry: an owner-authored whole-machine Function name in the intake
/// file, and which already-registered (`qlc-functions.toml`) per-zone Function each
/// zone's slice is written into. Zone scope is fixed by the state semantics already
/// settled elsewhere in this project (warning/working are eye-only; fault,
/// controller-fault, and ambient are whole-machine) -- this table does not invent
/// that, it encodes it.
struct ContractEntry {
    intake_name: &'static str,
    /// (zone name, registered target Function name)
    targets: &'static [(&'static str, &'static str)],
}

const CONTRACT: &[ContractEntry] = &[
    ContractEntry { intake_name: "Base Ambient", targets: &[("ram", "ambient_ram"), ("rog_eye", "ambient_eye"), ("strip", "ambient_strip")] },
    ContractEntry { intake_name: "State — Fault", targets: &[("ram", "fault_ram"), ("rog_eye", "fault_eye"), ("strip", "fault_strip")] },
    ContractEntry { intake_name: "State — Warning", targets: &[("rog_eye", "warning_eye")] },
    ContractEntry { intake_name: "State — Working", targets: &[("rog_eye", "working_eye")] },
    ContractEntry { intake_name: "State — Controller Fault", targets: &[("ram", "controller_fault_ram"), ("rog_eye", "controller_fault_eye"), ("strip", "controller_fault_strip")] },
];

fn intake_dir(root: &Path) -> PathBuf {
    root.join("../qlcplus/intake")
}

/// `monolithd workspace list`: the intake files available to select from.
fn list(root: &Path) -> Result<(), String> {
    let dir = intake_dir(root);
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map_err(|error| format!("read {}: {error}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().to_str().and_then(|name| name.strip_suffix(".qxw")).map(str::to_owned))
        .collect();
    names.sort();
    if names.is_empty() {
        println!("no intake workspaces in {}", dir.display());
        return Ok(());
    }
    println!("available intake workspaces:");
    for name in names {
        println!("  {name}");
    }
    Ok(())
}

struct FixtureVal {
    original: String,
    fixture_id: u32,
    triples: Vec<(u8, u8, u8)>,
}

fn function_block_by_id<'a>(xml: &'a str, id: u32) -> Result<&'a str, String> {
    let open = format!("<Function ID=\"{id}\" ");
    let start = xml.find(&open).ok_or_else(|| format!("Function {id} not found in the workspace"))?;
    let end = xml[start..].find("</Function>").ok_or_else(|| format!("Function {id}: no closing tag"))? + start + "</Function>".len();
    Ok(&xml[start..end])
}

fn function_block_by_name<'a>(xml: &'a str, name: &str) -> Result<&'a str, String> {
    let open = format!("Name=\"{name}\">");
    let name_at = xml.find(&open).ok_or_else(|| format!("Function {name:?} not found in the intake workspace"))?;
    let start = xml[..name_at].rfind("<Function ID=\"").ok_or_else(|| format!("Function {name:?}: malformed opening tag"))?;
    let end = xml[start..].find("</Function>").ok_or_else(|| format!("Function {name:?}: no closing tag"))? + start + "</Function>".len();
    Ok(&xml[start..end])
}

fn function_kind(block: &str) -> Result<String, String> {
    let marker = "Type=\"";
    let at = block.find(marker).ok_or("Function block has no Type attribute")? + marker.len();
    let end = block[at..].find('"').ok_or("malformed Type attribute")? + at;
    Ok(block[at..end].to_owned())
}

fn chaser_steps(block: &str) -> Vec<u32> {
    block
        .match_indices("<Step Number=")
        .filter_map(|(index, _)| {
            let tail = &block[index..];
            let gt = tail.find('>')?;
            let close = tail.find("</Step>")?;
            tail[gt + 1..close].parse().ok()
        })
        .collect()
}

fn chaser_speed_tag(block: &str) -> Result<&str, String> {
    let start = block.find("<Speed ").ok_or("Chaser has no Speed tag")?;
    let end = block[start..].find("/>").ok_or("malformed Speed tag")? + start + 2;
    Ok(&block[start..end])
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
        let numbers: Vec<u32> = tail[body_start..body_end]
            .split(',')
            .map(|token| token.trim().parse().map_err(|_| format!("bad channel/value token {token:?}")))
            .collect::<Result<_, String>>()?;
        if numbers.len() % 2 != 0 {
            return Err(format!("FixtureVal {fixture_id} has an odd channel/value count"));
        }
        let values: Vec<u8> = numbers.chunks(2).map(|pair| pair[1] as u8).collect();
        if values.len() % 3 != 0 {
            return Err(format!("FixtureVal {fixture_id} has {} channels, not a multiple of three", values.len()));
        }
        let triples = values.chunks(3).map(|c| (c[0], c[1], c[2])).collect();
        out.push(FixtureVal { original, fixture_id, triples });
        rest = &tail[end..];
    }
    Ok(out)
}

fn render_fixture_val(fixture_id: u32, triples: &[(u8, u8, u8)]) -> String {
    let mut channel = 0u32;
    let mut parts = Vec::with_capacity(triples.len() * 3);
    for (r, g, b) in triples {
        parts.push(format!("{channel},{r}"));
        channel += 1;
        parts.push(format!("{channel},{g}"));
        channel += 1;
        parts.push(format!("{channel},{b}"));
        channel += 1;
    }
    format!("<FixtureVal ID=\"{fixture_id}\">{}</FixtureVal>", parts.join(","))
}

/// Fixture ID -> zone name, for the intake workspace's own fixture patch, resolved
/// against the production registry's zone geometry (both are assumed to describe the
/// same physical rig).
fn zones_by_fixture(intake_workspace: &registry::Workspace, registry: &Registry) -> BTreeMap<u32, String> {
    intake_workspace
        .fixtures
        .iter()
        .filter_map(|(&id, fixture)| registry.zone_of(fixture.universe, fixture.address).map(|zone| (id, zone.to_owned())))
        .collect()
}

/// Replace every FixtureVal in the block for Function `target_id` with the subset of
/// `source_triples` (fixture_id -> triples) that the block's own FixtureVal fixture
/// IDs name -- the target's fixture set is authoritative, matching what's already
/// registered for that zone; this never adds or removes fixtures, only recolors.
fn rewrite_target(xml: &str, target_id: u32, source_triples: &BTreeMap<u32, Vec<(u8, u8, u8)>>, context: &str) -> Result<String, String> {
    let block = function_block_by_id(xml, target_id)?.to_owned();
    let target_vals = parse_fixture_vals(&block)?;
    let mut new_block = block.clone();
    for fv in &target_vals {
        let triples = source_triples
            .get(&fv.fixture_id)
            .ok_or_else(|| format!("{context}: intake source has no data for fixture {} that target Function {target_id} expects", fv.fixture_id))?;
        if triples.len() != fv.triples.len() {
            return Err(format!("{context}: fixture {} has {} LEDs in the intake source but {} in the target", fv.fixture_id, triples.len(), fv.triples.len()));
        }
        let new_tag = render_fixture_val(fv.fixture_id, triples);
        if new_block.matches(fv.original.as_str()).count() != 1 {
            return Err(format!("{context}: FixtureVal {} is not uniquely located in Function {target_id}; refusing to guess", fv.fixture_id));
        }
        new_block = new_block.replacen(fv.original.as_str(), &new_tag, 1);
    }
    if xml.matches(block.as_str()).count() != 1 {
        return Err(format!("{context}: Function {target_id} block is not uniquely located"));
    }
    Ok(xml.replacen(block.as_str(), &new_block, 1))
}

fn source_triples_for_zone(source_block: &str, zone: &str, zones: &BTreeMap<u32, String>) -> Result<BTreeMap<u32, Vec<(u8, u8, u8)>>, String> {
    let mut out = BTreeMap::new();
    for fv in parse_fixture_vals(source_block)? {
        if zones.get(&fv.fixture_id).map(String::as_str) == Some(zone) {
            out.insert(fv.fixture_id, fv.triples);
        }
    }
    Ok(out)
}

/// Apply one contract entry (Scene or Chaser) from the intake workspace into the
/// production workspace, per zone.
fn apply_entry(mut production: String, entry: &ContractEntry, intake_xml: &str, intake_workspace: &registry::Workspace, registry: &Registry) -> Result<String, String> {
    let zones = zones_by_fixture(intake_workspace, registry);
    let source_block = function_block_by_name(intake_xml, entry.intake_name)?;
    let kind = function_kind(source_block)?;

    for &(zone, target_name) in entry.targets {
        let target = registry.function(target_name).ok_or_else(|| format!("{target_name} is not registered in qlc-functions.toml"))?;
        let context = format!("{} -> {target_name}", entry.intake_name);

        if kind.eq_ignore_ascii_case("Scene") {
            let triples = source_triples_for_zone(source_block, zone, &zones)?;
            if triples.is_empty() {
                return Err(format!("{context}: intake Function {:?} writes nothing in zone {zone}", entry.intake_name));
            }
            production = rewrite_target(&production, target.id, &triples, &context)?;
        } else if kind.eq_ignore_ascii_case("Chaser") {
            let intake_steps = chaser_steps(source_block);
            if intake_steps.len() != target.children.len() {
                return Err(format!(
                    "{context}: intake chaser {:?} has {} steps but the registered target {target_name} expects {} -- step count is fixed by the contract, only content may change",
                    entry.intake_name,
                    intake_steps.len(),
                    target.children.len()
                ));
            }
            for (intake_child_id, &target_child_id) in intake_steps.iter().zip(target.children.iter()) {
                let child_block = function_block_by_id(intake_xml, *intake_child_id)?;
                let triples = source_triples_for_zone(child_block, zone, &zones)?;
                if triples.is_empty() {
                    return Err(format!("{context}: intake step {intake_child_id} writes nothing in zone {zone}"));
                }
                production = rewrite_target(&production, target_child_id, &triples, &format!("{context} step {intake_child_id}"))?;
            }
            // The animation timing is also the owner's to set, per the intake contract.
            let speed_tag = chaser_speed_tag(source_block)?.to_owned();
            let target_block = function_block_by_id(&production, target.id)?.to_owned();
            let target_speed = chaser_speed_tag(&target_block)?.to_owned();
            if target_speed != speed_tag {
                let new_target_block = target_block.replacen(&target_speed, &speed_tag, 1);
                if production.matches(target_block.as_str()).count() != 1 {
                    return Err(format!("{context}: Function {} block is not uniquely located", target.id));
                }
                production = production.replacen(target_block.as_str(), &new_target_block, 1);
            }
        } else {
            return Err(format!("{context}: intake Function {:?} is a {kind}, expected Scene or Chaser", entry.intake_name));
        }
    }
    Ok(production)
}

/// `monolithd workspace select NAME [--check]`.
pub fn run(arguments: Vec<String>) -> Result<(), String> {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    let usage = "usage: monolithd workspace <list|select NAME [--check]>";

    let mut rest = arguments.into_iter();
    match rest.next().as_deref() {
        Some("list") => return list(&root),
        Some("select") => {}
        _ => return Err(usage.to_owned()),
    }
    let remaining: Vec<String> = rest.collect();
    let check_only = remaining.iter().any(|a| a == "--check");
    let name = remaining.iter().find(|a| a.as_str() != "--check").ok_or(usage)?;

    let registry_path = root.join("qlc-functions.toml");
    let registry = registry::load(&registry_path)?;
    let production_path = registry.workspace_path(&registry_path);

    let intake_path = intake_dir(&root).join(format!("{name}.qxw"));
    let intake_xml = std::fs::read_to_string(&intake_path).map_err(|error| format!("read {}: {error}", intake_path.display()))?;
    let intake_workspace = registry::parse_workspace(&intake_xml)?;

    let original = std::fs::read_to_string(&production_path).map_err(|error| format!("read {}: {error}", production_path.display()))?;
    let mut production = original.clone();
    for entry in CONTRACT {
        production = apply_entry(production, entry, &intake_xml, &intake_workspace, &registry)?;
    }

    if production == original {
        println!("no change: the production workspace already matches {name:?}");
        return Ok(());
    }

    println!("{} from {name:?}:", if check_only { "would update" } else { "updating" });
    for entry in CONTRACT {
        println!("  {} -> {}", entry.intake_name, entry.targets.iter().map(|(_, t)| *t).collect::<Vec<_>>().join(", "));
    }

    if check_only {
        return Ok(());
    }

    let backup_dir = std::path::PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?).join(".local/share/monolith-events/backups");
    std::fs::create_dir_all(&backup_dir).map_err(|error| format!("create {}: {error}", backup_dir.display()))?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|error| error.to_string())?.as_secs();
    let backup_path = backup_dir.join(format!("monolith-lighting.qxw.{stamp}.before-workspace-select-{name}"));
    std::fs::write(&backup_path, &original).map_err(|error| format!("write backup {}: {error}", backup_path.display()))?;
    println!("backup: {}", backup_path.display());

    std::fs::write(&production_path, &production).map_err(|error| format!("write {}: {error}", production_path.display()))?;
    println!("wrote {}", production_path.display());
    println!("next: `monolithd validate-registry`, then restart monolith-lighting-stack.service for QLC+ to reload it.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intake_and_registry() -> (String, Registry, registry::Workspace) {
        // Two fixtures: 0 in zone "a", 1 in zone "b". One flat Scene ("State — X")
        // spanning both, one 2-step Chaser ("Base Ambient") spanning both.
        let intake = concat!(
            "  <Function ID=\"1\" Type=\"Scene\" Name=\"State — X\">\n",
            "   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"0\"/>\n",
            "   <FixtureVal ID=\"0\">0,10,1,20,2,30</FixtureVal>\n",
            "   <FixtureVal ID=\"1\">0,40,1,50,2,60</FixtureVal>\n",
            "  </Function>\n",
            "  <Function ID=\"2\" Type=\"Scene\" Name=\"Base Ambient — Low\">\n",
            "   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"0\"/>\n",
            "   <FixtureVal ID=\"0\">0,1,1,2,2,3</FixtureVal>\n",
            "   <FixtureVal ID=\"1\">0,4,1,5,2,6</FixtureVal>\n",
            "  </Function>\n",
            "  <Function ID=\"3\" Type=\"Scene\" Name=\"Base Ambient — High\">\n",
            "   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"0\"/>\n",
            "   <FixtureVal ID=\"0\">0,7,1,8,2,9</FixtureVal>\n",
            "   <FixtureVal ID=\"1\">0,10,1,11,2,12</FixtureVal>\n",
            "  </Function>\n",
            "  <Function ID=\"4\" Type=\"Chaser\" Name=\"Base Ambient\">\n",
            "   <Speed FadeIn=\"900\" FadeOut=\"900\" Duration=\"1500\"/>\n",
            "   <Direction>Forward</Direction>\n",
            "   <RunOrder>Loop</RunOrder>\n",
            "   <Step Number=\"0\">2</Step>\n",
            "   <Step Number=\"1\">3</Step>\n",
            "  </Function>\n",
        )
        .to_owned();

        let mut registry_toml = String::new();
        registry_toml += "version = 1\nworkspace = \"prod.qxw\"\n";
        registry_toml += "[zones.a]\nregions = [{ universe = 0, address = 0, channels = 3 }]\n";
        registry_toml += "[zones.b]\nregions = [{ universe = 0, address = 3, channels = 3 }]\n";
        registry_toml += "[[functions]]\nname = \"target_x_a\"\nkind = \"scene\"\nid = 101\nzones = [\"a\"]\ncomposable = true\n";
        registry_toml += "[[functions]]\nname = \"target_ambient_a\"\nkind = \"chaser\"\nid = 104\nchildren = [102, 103]\nzones = [\"a\"]\ncomposable = true\n";
        let registry: Registry = toml::from_str(&registry_toml).unwrap();

        let mut fixtures = std::collections::BTreeMap::new();
        fixtures.insert(0, registry::Fixture { universe: 0, address: 0, channels: 3 });
        fixtures.insert(1, registry::Fixture { universe: 0, address: 3, channels: 3 });
        let workspace = registry::Workspace { fixtures, functions: Default::default() };

        (intake, registry, workspace)
    }

    #[test]
    fn splits_a_flat_scene_by_zone_and_recolors_only_the_target_fixture() {
        let (intake, registry, workspace) = intake_and_registry();
        let entry = ContractEntry { intake_name: "State — X", targets: &[("a", "target_x_a")] };

        let mut production = String::new();
        production += "  <Function ID=\"101\" Type=\"Scene\" Name=\"target_x_a\">\n";
        production += "   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"0\"/>\n";
        production += "   <FixtureVal ID=\"0\">0,255,1,255,2,255</FixtureVal>\n";
        production += "  </Function>\n";

        let result = apply_entry(production, &entry, &intake, &workspace, &registry).unwrap();
        assert!(result.contains("<FixtureVal ID=\"0\">0,10,1,20,2,30</FixtureVal>"), "{result}");
    }

    #[test]
    fn splits_a_chaser_recoloring_both_steps_and_carrying_the_timing() {
        let (intake, registry, workspace) = intake_and_registry();
        let entry = ContractEntry { intake_name: "Base Ambient", targets: &[("a", "target_ambient_a")] };

        let mut production = String::new();
        production += "  <Function ID=\"102\" Type=\"Scene\" Name=\"lo\">\n   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"0\"/>\n   <FixtureVal ID=\"0\">0,0,1,0,2,0</FixtureVal>\n  </Function>\n";
        production += "  <Function ID=\"103\" Type=\"Scene\" Name=\"hi\">\n   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"0\"/>\n   <FixtureVal ID=\"0\">0,0,1,0,2,0</FixtureVal>\n  </Function>\n";
        production += "  <Function ID=\"104\" Type=\"Chaser\" Name=\"target_ambient_a\">\n   <Speed FadeIn=\"1200\" FadeOut=\"1200\" Duration=\"1800\"/>\n   <Direction>Forward</Direction>\n   <RunOrder>Loop</RunOrder>\n   <Step Number=\"0\">102</Step>\n   <Step Number=\"1\">103</Step>\n  </Function>\n";

        let result = apply_entry(production, &entry, &intake, &workspace, &registry).unwrap();
        assert!(result.contains("<FixtureVal ID=\"0\">0,1,1,2,2,3</FixtureVal>"), "low step recolored: {result}");
        assert!(result.contains("<FixtureVal ID=\"0\">0,7,1,8,2,9</FixtureVal>"), "high step recolored: {result}");
        assert!(result.contains("<Speed FadeIn=\"900\" FadeOut=\"900\" Duration=\"1500\"/>"), "timing carried from intake: {result}");
    }

    #[test]
    fn a_chaser_step_count_mismatch_is_refused_not_guessed() {
        let (intake, mut registry, workspace) = intake_and_registry();
        registry.functions[1].children = vec![102, 103, 105]; // target expects 3 steps, intake has 2
        let entry = ContractEntry { intake_name: "Base Ambient", targets: &[("a", "target_ambient_a")] };
        let production = "  <Function ID=\"104\" Type=\"Chaser\" Name=\"target_ambient_a\">\n   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"0\"/>\n  </Function>\n".to_owned();
        assert!(apply_entry(production, &entry, &intake, &workspace, &registry).is_err());
    }
}
