//! Build the production QLC+ workspace from a whole-machine "intake" workspace the
//! owner authors directly in QLC+'s own GUI -- one cohesive Function per state, every
//! zone together, exactly the way the owner wants to author it. This module never
//! decides a color or a timing value: everything in its output is the owner's own
//! FixtureVal/Speed data from the intake file, filtered by zone. RGB Matrices are
//! copied as independent per-zone assets with their authored parameters. See "Named-function
//! intake contract" in the handoff note for why (QLC+'s API can only start/stop a
//! Function that already exists by ID in the loaded workspace -- it cannot create or
//! partially start one -- so independent per-zone control requires the per-zone
//! pieces to physically exist before the stack boots, and *something* has to produce
//! them from the owner's one combined Function).
//!
//! The owner-facing contract: the Functions named in `CONTRACT` below are the
//! required whole-machine state and ambient inputs. They may be edited in QLC+'s
//! GUI, but not renamed or made to write outside their registered zones. Any
//! additional RGB Matrix is imported as an independent named asset when its fixture
//! group exactly matches production and belongs to one zone. Other extra Functions
//! (reference Scenes, works in progress, etc.) are ignored.

use crate::registry::{self, Registry};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// One contract entry: an owner-authored whole-machine Function name in the intake
/// file, and which already-registered (`qlc-functions.toml`) per-zone Function each
/// zone's slice is written into. As of 2026-09-22 every contract entry is
/// whole-machine-authorable (ram, rog_eye, strip) -- which zone a warning or the
/// working indicator actually *displays on* at runtime is a separate, owner-editable
/// policy in `controller.toml` (`warning_zones`/`working_zones`), not fixed here.
/// This table only says what the owner is allowed to author and where it can land if
/// asked for; it does not decide when it's shown.
struct ContractEntry {
    intake_name: &'static str,
    /// (zone name, registered target Function name)
    targets: &'static [(&'static str, &'static str)],
}

const CONTRACT: &[ContractEntry] = &[
    ContractEntry { intake_name: "Base Ambient", targets: &[("ram", "ambient_ram"), ("rog_eye", "ambient_eye"), ("strip", "ambient_strip")] },
    ContractEntry { intake_name: "State — Fault", targets: &[("ram", "fault_ram"), ("rog_eye", "fault_eye"), ("strip", "fault_strip")] },
    ContractEntry { intake_name: "State — Warning", targets: &[("ram", "warning_ram"), ("rog_eye", "warning_eye"), ("strip", "warning_strip")] },
    ContractEntry { intake_name: "State — Working", targets: &[("ram", "working_ram"), ("rog_eye", "working_eye"), ("strip", "working_strip")] },
    ContractEntry { intake_name: "State — Controller Fault", targets: &[("ram", "controller_fault_ram"), ("rog_eye", "controller_fault_eye"), ("strip", "controller_fault_strip")] },
    ContractEntry { intake_name: "State — Pre-Sleep", targets: &[("ram", "pre_sleep_ram"), ("rog_eye", "pre_sleep_eye"), ("strip", "pre_sleep_strip")] },
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

const MATRIX_BEGIN: &str = "# BEGIN MONOLITHD RGB MATRIX ASSETS";
const MATRIX_END: &str = "# END MONOLITHD RGB MATRIX ASSETS";

#[derive(serde::Deserialize)]
struct ManagedMatrices {
    functions: Vec<registry::FunctionEntry>,
}

fn managed_matrix_section(registry_toml: &str) -> Result<(String, Vec<registry::FunctionEntry>), String> {
    match (registry_toml.find(MATRIX_BEGIN), registry_toml.find(MATRIX_END)) {
        (None, None) => Ok((registry_toml.to_owned(), Vec::new())),
        (Some(start), Some(end)) if start < end => {
            let after = end + MATRIX_END.len();
            let section = &registry_toml[start + MATRIX_BEGIN.len()..end];
            let entries: ManagedMatrices = toml::from_str(section).map_err(|e| format!("parse managed RGB Matrix registry section: {e}"))?;
            if entries.functions.iter().any(|entry| !entry.kind.eq_ignore_ascii_case("rgbmatrix") || !entry.name.starts_with("matrix:")) {
                return Err("managed RGB Matrix registry section contains a non-matrix asset".to_owned());
            }
            let mut base = registry_toml[..start].to_owned();
            base.push_str(&registry_toml[after..]);
            Ok((base.trim_end().to_owned() + "\n", entries.functions))
        }
        _ => Err("malformed managed RGB Matrix registry section markers".to_owned()),
    }
}

fn fixture_group_block_by_id<'a>(xml: &'a str, id: u32) -> Result<&'a str, String> {
    let open = format!("<FixtureGroup ID=\"{id}\">");
    let start = xml.find(&open).ok_or_else(|| format!("fixture group {id} not found"))?;
    let end = xml[start..].find("</FixtureGroup>").ok_or_else(|| format!("fixture group {id} has no closing tag"))? + start + "</FixtureGroup>".len();
    Ok(&xml[start..end])
}

fn matrix_zone(group_id: u32, workspace: &registry::Workspace, registry: &Registry) -> Result<String, String> {
    let members = workspace.fixture_groups.get(&group_id).ok_or_else(|| format!("RGB Matrix references unknown fixture group {group_id}"))?;
    if members.is_empty() {
        return Err(format!("RGB Matrix fixture group {group_id} is empty"));
    }
    let mut zones = BTreeSet::new();
    for fixture_id in members {
        let fixture = workspace.fixtures.get(fixture_id).ok_or_else(|| format!("RGB Matrix fixture group {group_id} references unknown fixture {fixture_id}"))?;
        if fixture.channels != 3 {
            return Err(format!("RGB Matrix fixture group {group_id} contains fixture {fixture_id} with {} channels; expected one RGB LED per fixture", fixture.channels));
        }
        for channel in 0..fixture.channels {
            let zone = registry.zone_of(fixture.universe, fixture.address + channel).ok_or_else(|| format!("RGB Matrix fixture {fixture_id} writes outside registered zones"))?;
            zones.insert(zone.to_owned());
        }
    }
    if zones.len() != 1 {
        return Err(format!("RGB Matrix fixture group {group_id} spans zones {zones:?}; import one zone per Matrix"));
    }
    Ok(zones.into_iter().next().unwrap())
}

/// Copy each named RGB Matrix from the intake workspace as an independently selectable
/// asset. Its fixture group must be identical in both workspaces so QLC's spatial
/// algorithm renders the same pixels. Generated IDs and registry entries remain stable
/// across repeated selects; removing a Matrix from intake removes its managed asset.
fn import_matrices(
    production: String,
    intake_xml: &str,
    intake_workspace: &registry::Workspace,
    registry: &Registry,
    registry_toml: &str,
) -> Result<(String, String, Vec<String>), String> {
    let (base, old_managed) = managed_matrix_section(registry_toml)?;
    let production_workspace = registry::parse_workspace(&production)?;
    let mut old_by_name = BTreeMap::new();
    let mut result = production;
    for entry in old_managed {
        if registry.function(&entry.name).map(|current| current.id) != Some(entry.id) {
            return Err(format!("managed RGB Matrix {} no longer matches the live registry", entry.name));
        }
        let old_block = function_block_by_id(&result, entry.id)?.to_owned();
        if !function_kind(&old_block)?.eq_ignore_ascii_case("RGBMatrix") {
            return Err(format!("managed asset {} is no longer an RGB Matrix", entry.name));
        }
        let line = format!("  {old_block}\n");
        if !result.contains(&line) {
            return Err(format!("managed RGB Matrix {} has unexpected XML layout", entry.name));
        }
        result = result.replacen(&line, "", 1);
        old_by_name.insert(entry.name, entry.id);
    }

    let mut used_ids: BTreeSet<u32> = production_workspace.functions.keys().copied().collect();
    let mut entries = Vec::new();
    let mut labels = Vec::new();
    let mut names = BTreeSet::new();
    for (&intake_id, function) in &intake_workspace.functions {
        if !function.kind.eq_ignore_ascii_case("RGBMatrix") {
            continue;
        }
        if function.name.trim().is_empty() {
            return Err(format!("RGB Matrix Function {intake_id} has no name"));
        }
        let name = format!("matrix:{}", function.name);
        if !names.insert(name.clone()) {
            return Err(format!("duplicate RGB Matrix name {:?} in intake", function.name));
        }
        if registry.function(&name).is_some() && !old_by_name.contains_key(&name) {
            return Err(format!("RGB Matrix asset {name:?} conflicts with an existing registered Function"));
        }
        let group_id = function.fixture_group.ok_or_else(|| format!("RGB Matrix {:?} has no fixture group", function.name))?;
        let intake_group = fixture_group_block_by_id(intake_xml, group_id)?;
        let production_group = fixture_group_block_by_id(&result, group_id)?;
        if intake_group != production_group {
            return Err(format!("RGB Matrix {:?}: fixture group {group_id} differs from production; preserve its pixel layout", function.name));
        }
        let zone = matrix_zone(group_id, intake_workspace, registry)?;
        let id = if let Some(&old_id) = old_by_name.get(&name) {
            old_id
        } else {
            let next = used_ids.iter().next_back().copied().unwrap_or(0).checked_add(1).ok_or("no free Function ID for RGB Matrix")?;
            next
        };
        if !used_ids.insert(id) && !old_by_name.values().any(|old_id| *old_id == id) {
            return Err(format!("RGB Matrix {name:?} Function ID {id} collides with production"));
        }
        let source = function_block_by_id(intake_xml, intake_id)?;
        let old_open = format!("<Function ID=\"{intake_id}\"");
        let new_open = format!("<Function ID=\"{id}\"");
        let imported = source.replacen(&old_open, &new_open, 1);
        let engine_end = result.rfind(" </Engine>").ok_or("production workspace has no Engine closing tag")?;
        result.insert_str(engine_end, &format!("  {imported}\n"));
        labels.push(format!("  {} -> Function {id} ({zone})", function.name));
        entries.push((name, id, zone));
    }

    let mut registry_out = base.trim_end().to_owned();
    registry_out.push('\n');
    if !entries.is_empty() {
        registry_out.push_str("\n");
        registry_out.push_str(MATRIX_BEGIN);
        registry_out.push('\n');
        for (name, id, zone) in entries {
            registry_out.push_str("[[functions]]\n");
            registry_out.push_str(&format!("name = {}\n", toml::Value::String(name).to_string()));
            registry_out.push_str("kind = \"rgbmatrix\"\n");
            registry_out.push_str(&format!("id = {id}\n"));
            registry_out.push_str("children = []\n");
            registry_out.push_str(&format!("zones = [{}]\n", toml::Value::String(zone).to_string()));
            registry_out.push_str("composable = true\n\n");
        }
        registry_out.push_str(MATRIX_END);
        registry_out.push('\n');
    }
    Ok((result, registry_out, labels))
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
/// Naming convention for progress-zone keyframes in the intake file: one
/// owner-authored "Empty"/"Full" pair per zone, shared by every progress family
/// registered for that zone (e.g. progress_ram and progress_ram_interleaved both
/// reuse "Progress RAM — Empty"/"Progress RAM — Full" -- they differ only in fill
/// order, which is measured from production, never authored twice).
fn progress_keyframe_names(zone: &str) -> (String, String) {
    let label = match zone {
        "ram" => "RAM",
        "rog_eye" => "Eye",
        "strip" => "Strip",
        other => other,
    };
    (format!("Progress {label} — Empty"), format!("Progress {label} — Full"))
}

/// A single fill position: which FixtureVal (by fixture ID) and which triple within
/// it. Positions are ordered by the step at which they first turn "complete".
type Position = (u32, usize);

/// Derive a progress family's fill order by diffing each of its already-authored
/// production steps against the previous step -- measured, never hand-encoded, so a
/// change to the physical fill geometry never needs a matching code change here.
/// Exactly one position must newly differ per step, since "Progress X of N" is
/// authored as N discrete single-position increments; anything else means this
/// family isn't shaped the way this function assumes, and it refuses rather than
/// guessing.
fn derive_fill_order(xml: &str, first_id: u32, total: u32) -> Result<Vec<Position>, String> {
    let mut previous = parse_fixture_vals(function_block_by_id(xml, first_id)?)?;
    let mut order = Vec::new();
    for step in 1..=total {
        let current = parse_fixture_vals(function_block_by_id(xml, first_id + step)?)?;
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

/// Rewrite a progress family's steps from two owner-authored keyframe Scenes --
/// full per-pixel content, not flat colors -- swapping each position from its
/// "Empty" to "Full" keyframe color at the position's own measured fill step. No
/// interpolation: a discrete fill, matching the physical bar-fill look already in
/// production (per the owner's choice, 2026-09-22), just recolored from
/// QLC+-authored endpoints instead of a hand-picked flat color.
fn apply_progress_family(xml: &str, first_id: u32, total: u32, empty_block: &str, full_block: &str, context: &str) -> Result<String, String> {
    let order = derive_fill_order(xml, first_id, total)?;
    let empty = parse_fixture_vals(empty_block)?;
    let full = parse_fixture_vals(full_block)?;
    if empty.len() != full.len() {
        return Err(format!("{context}: Empty/Full keyframes have different fixture counts"));
    }

    let mut per_position_colors: Vec<(u32, Vec<(u8, u8, u8)>, Vec<(u8, u8, u8)>)> = Vec::with_capacity(empty.len());
    for (e, f) in empty.iter().zip(full.iter()) {
        if e.fixture_id != f.fixture_id {
            return Err(format!("{context}: keyframe fixture order mismatch ({} vs {})", e.fixture_id, f.fixture_id));
        }
        if e.triples.len() != f.triples.len() {
            return Err(format!("{context}: fixture {}: Empty/Full keyframe LED counts differ", e.fixture_id));
        }
        per_position_colors.push((e.fixture_id, e.triples.clone(), f.triples.clone()));
    }

    let mut result = xml.to_owned();
    for step in 0..=total {
        let complete_positions: std::collections::HashSet<Position> = order[..step as usize].iter().copied().collect();
        let mut per_fixture: Vec<(u32, Vec<(u8, u8, u8)>)> = Vec::with_capacity(per_position_colors.len());
        for (fixture_id, empty_triples, full_triples) in &per_position_colors {
            let triples = (0..empty_triples.len())
                .map(|index| if complete_positions.contains(&(*fixture_id, index)) { full_triples[index] } else { empty_triples[index] })
                .collect();
            per_fixture.push((*fixture_id, triples));
        }

        let id = first_id + step;
        let block = function_block_by_id(&result, id)?.to_owned();
        let current = parse_fixture_vals(&block)?;
        let mut new_block = block.clone();
        for (fv, (fixture_id, triples)) in current.iter().zip(per_fixture.iter()) {
            if fv.fixture_id != *fixture_id {
                return Err(format!("{context}: step {step}: fixture order drifted mid-rewrite"));
            }
            let new_tag = render_fixture_val(*fixture_id, triples);
            if new_block.matches(fv.original.as_str()).count() != 1 {
                return Err(format!("{context}: step {step}: FixtureVal {fixture_id} is not uniquely located; refusing to guess"));
            }
            new_block = new_block.replacen(fv.original.as_str(), &new_tag, 1);
        }
        if result.matches(block.as_str()).count() != 1 {
            return Err(format!("{context}: step {step} (Function {id}): block is not uniquely located"));
        }
        result = result.replacen(block.as_str(), &new_block, 1);
    }
    Ok(result)
}

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
    let root = crate::paths::config_dir();
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
    let registry_original = std::fs::read_to_string(&registry_path).map_err(|error| format!("read {}: {error}", registry_path.display()))?;
    let registry = registry::load(&registry_path)?;
    let production_path = registry.workspace_path(&registry_path);

    let intake_path = intake_dir(&root).join(format!("{name}.qxw"));
    let intake_xml = std::fs::read_to_string(&intake_path).map_err(|error| format!("read {}: {error}", intake_path.display()))?;
    let intake_workspace = registry::parse_workspace(&intake_xml)?;

    let original = std::fs::read_to_string(&production_path).map_err(|error| format!("read {}: {error}", production_path.display()))?;
    let mut production = original.clone();
    let mut touched: Vec<String> = Vec::new();
    for entry in CONTRACT {
        production = apply_entry(production, entry, &intake_xml, &intake_workspace, &registry)?;
        touched.push(format!("  {} -> {}", entry.intake_name, entry.targets.iter().map(|(_, t)| *t).collect::<Vec<_>>().join(", ")));
    }
    for progress in &registry.progress {
        let (empty_name, full_name) = progress_keyframe_names(&progress.zone);
        let context = format!("{} (zone {})", progress.name, progress.zone);
        let empty_block = function_block_by_name(&intake_xml, &empty_name)
            .map_err(|_| format!("{context}: intake is missing {empty_name:?} -- every progress zone needs an Empty/Full keyframe pair"))?;
        let full_block = function_block_by_name(&intake_xml, &full_name)
            .map_err(|_| format!("{context}: intake is missing {full_name:?} -- every progress zone needs an Empty/Full keyframe pair"))?;
        production = apply_progress_family(&production, progress.first_id, progress.total, empty_block, full_block, &context)?;
        touched.push(format!("  {} -> Functions {}-{} <- {empty_name:?} / {full_name:?}", context, progress.first_id, progress.first_id + progress.total));
    }

    let (candidate, registry_candidate, matrices) = import_matrices(production, &intake_xml, &intake_workspace, &registry, &registry_original)?;
    production = candidate;
    touched.extend(matrices);
    let mut candidate_registry: Registry = toml::from_str(&registry_candidate).map_err(|error| format!("candidate registry TOML: {error}"))?;
    let candidate_workspace = registry::parse_workspace(&production)?;
    candidate_registry.attach_periods(&candidate_workspace);
    let problems = candidate_registry.validate(&candidate_workspace);
    if !problems.is_empty() {
        return Err(format!("candidate workspace/registry failed validation:\n{}", problems.join("\n")));
    }

    if production == original && registry_candidate == registry_original {
        println!("no change: the production workspace already matches {name:?}");
        return Ok(());
    }

    println!("{} from {name:?}:", if check_only { "would update" } else { "updating" });
    for line in &touched {
        println!("{line}");
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

    if registry_candidate != registry_original {
        let registry_backup = backup_dir.join(format!("qlc-functions.toml.{stamp}.before-workspace-select-{name}"));
        std::fs::write(&registry_backup, &registry_original).map_err(|error| format!("write backup {}: {error}", registry_backup.display()))?;
        println!("backup: {}", registry_backup.display());
    }

    std::fs::write(&production_path, &production).map_err(|error| format!("write {}: {error}", production_path.display()))?;
    println!("wrote {}", production_path.display());
    if registry_candidate != registry_original {
        if let Err(error) = std::fs::write(&registry_path, &registry_candidate) {
            std::fs::write(&production_path, &original).map_err(|rollback| format!("write registry failed: {error}; production rollback failed: {rollback}"))?;
            return Err(format!("write {}: {error}; production workspace restored", registry_path.display()));
        }
        println!("wrote {}", registry_path.display());
    }
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
        let workspace = registry::Workspace { fixtures, fixture_groups: Default::default(), functions: Default::default() };

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

    // ------------------------------------------------------------ progress keyframes

    /// A 3-position "Progress Test 00..03 of 3" family, filled in fixture order
    /// 0, 1, 2, matching the shape real production progress families use.
    fn sample_progress_family(fill_color: (u8, u8, u8)) -> String {
        let mut xml = String::new();
        for step in 0..=3u32 {
            let complete_upto = step as usize;
            let mut fixtures = String::new();
            for fixture in 0..3u32 {
                let (r, g, b) = if (fixture as usize) < complete_upto { fill_color } else { (255, 255, 255) };
                fixtures += &format!("   <FixtureVal ID=\"{fixture}\">0,{r},1,{g},2,{b}</FixtureVal>\n");
            }
            xml += &format!("  <Function ID=\"{step}\" Type=\"Scene\" Name=\"Progress Test {step:02} of 3\">\n   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"0\"/>\n{fixtures}  </Function>\n");
        }
        xml
    }

    #[test]
    fn derives_the_fill_order_from_real_production_data() {
        let xml = sample_progress_family((0, 255, 0));
        let order = derive_fill_order(&xml, 0, 3).unwrap();
        assert_eq!(order, vec![(0, 0), (1, 0), (2, 0)]);
    }

    #[test]
    fn a_family_that_is_not_a_simple_one_position_per_step_fill_is_refused() {
        // Two positions change between steps 0 and 1 -- not a shape this function
        // understands, so it must refuse rather than guess an order.
        let mut xml = String::new();
        xml += "  <Function ID=\"0\" Type=\"Scene\" Name=\"Bad 00\">\n   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"0\"/>\n   <FixtureVal ID=\"0\">0,255,1,255,2,255</FixtureVal>\n   <FixtureVal ID=\"1\">0,255,1,255,2,255</FixtureVal>\n  </Function>\n";
        xml += "  <Function ID=\"1\" Type=\"Scene\" Name=\"Bad 01\">\n   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"0\"/>\n   <FixtureVal ID=\"0\">0,0,1,255,2,0</FixtureVal>\n   <FixtureVal ID=\"1\">0,0,1,255,2,0</FixtureVal>\n  </Function>\n";
        assert!(derive_fill_order(&xml, 0, 1).is_err());
    }

    #[test]
    fn applies_owner_authored_keyframes_preserving_the_measured_fill_order() {
        let xml = sample_progress_family((0, 255, 0)); // production fill order: 0, 1, 2
        let empty = concat!(
            "  <Function ID=\"90\" Type=\"Scene\" Name=\"Progress Test — Empty\">\n",
            "   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"0\"/>\n",
            "   <FixtureVal ID=\"0\">0,50,1,0,2,255</FixtureVal>\n",
            "   <FixtureVal ID=\"1\">0,50,1,0,2,255</FixtureVal>\n",
            "   <FixtureVal ID=\"2\">0,50,1,0,2,255</FixtureVal>\n",
            "  </Function>\n"
        );
        let full = concat!(
            "  <Function ID=\"91\" Type=\"Scene\" Name=\"Progress Test — Full\">\n",
            "   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"0\"/>\n",
            "   <FixtureVal ID=\"0\">0,150,1,0,2,255</FixtureVal>\n",
            "   <FixtureVal ID=\"1\">0,150,1,0,2,255</FixtureVal>\n",
            "   <FixtureVal ID=\"2\">0,150,1,0,2,255</FixtureVal>\n",
            "  </Function>\n"
        );
        let result = apply_progress_family(&xml, 0, 3, empty, full, "test").unwrap();
        // Step 0: every position still the Empty keyframe's own color.
        let step0 = function_block_by_id(&result, 0).unwrap();
        assert!(step0.contains("<FixtureVal ID=\"0\">0,50,1,0,2,255</FixtureVal>"));
        assert!(step0.contains("<FixtureVal ID=\"2\">0,50,1,0,2,255</FixtureVal>"));
        // Step 2: positions 0 and 1 (measured fill order) are Full's color, 2 still Empty's.
        let step2 = function_block_by_id(&result, 2).unwrap();
        assert!(step2.contains("<FixtureVal ID=\"0\">0,150,1,0,2,255</FixtureVal>"));
        assert!(step2.contains("<FixtureVal ID=\"1\">0,150,1,0,2,255</FixtureVal>"));
        assert!(step2.contains("<FixtureVal ID=\"2\">0,50,1,0,2,255</FixtureVal>"));
        // Step 3: fully complete.
        let step3 = function_block_by_id(&result, 3).unwrap();
        assert!(step3.contains("<FixtureVal ID=\"2\">0,150,1,0,2,255</FixtureVal>"));
    }

    #[test]
    fn reapplying_keyframes_matching_current_production_is_idempotent() {
        let xml = sample_progress_family((0, 255, 0));
        let empty = "  <Function ID=\"90\" Type=\"Scene\" Name=\"X\">\n   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"0\"/>\n   <FixtureVal ID=\"0\">0,255,1,255,2,255</FixtureVal>\n   <FixtureVal ID=\"1\">0,255,1,255,2,255</FixtureVal>\n   <FixtureVal ID=\"2\">0,255,1,255,2,255</FixtureVal>\n  </Function>\n";
        let full = "  <Function ID=\"91\" Type=\"Scene\" Name=\"Y\">\n   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"0\"/>\n   <FixtureVal ID=\"0\">0,0,1,255,2,0</FixtureVal>\n   <FixtureVal ID=\"1\">0,0,1,255,2,0</FixtureVal>\n   <FixtureVal ID=\"2\">0,0,1,255,2,0</FixtureVal>\n  </Function>\n";
        let result = apply_progress_family(&xml, 0, 3, empty, full, "test").unwrap();
        assert_eq!(result, xml, "re-encoding the same colors already in production must reproduce byte-identical XML");
    }

    #[test]
    fn progress_keyframe_names_uses_the_registrys_zone_labels() {
        assert_eq!(progress_keyframe_names("ram"), ("Progress RAM — Empty".to_owned(), "Progress RAM — Full".to_owned()));
        assert_eq!(progress_keyframe_names("strip"), ("Progress Strip — Empty".to_owned(), "Progress Strip — Full".to_owned()));
    }

    #[test]
    fn imports_rgb_matrix_as_stable_registered_zone_asset() {
        let header = concat!(
            "<Workspace><Engine>\n",
            "  <Fixture><ID>43</ID><Universe>4</Universe><Address>15</Address><Channels>3</Channels></Fixture>\n",
            "  <FixtureGroup ID=\"37\"><Name>Strip</Name><Size X=\"1\" Y=\"1\"/><Head X=\"0\" Y=\"0\" Fixture=\"43\">0</Head></FixtureGroup>\n",
        );
        let production = format!(
            "{header}  <Function ID=\"10\" Type=\"Scene\" Name=\"Existing\"><FixtureVal ID=\"43\">0,1,1,2,2,3</FixtureVal></Function>\n </Engine>\n</Workspace>\n"
        );
        let intake = format!(
            "{header}  <Function ID=\"9\" Type=\"RGBMatrix\" Name=\"Strip Gradient\"><Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"200\"/><FixtureGroup>37</FixtureGroup><Color Index=\"0\">4294901760</Color></Function>\n </Engine>\n</Workspace>\n"
        );
        let registry_text = "version = 1\nworkspace = \"prod.qxw\"\n[zones.strip]\nregions = [{ universe = 4, address = 15, channels = 3 }]\n";
        let registry: Registry = toml::from_str(registry_text).unwrap();
        let intake_workspace = registry::parse_workspace(&intake).unwrap();
        let (result, registry_result, labels) = import_matrices(production.clone(), &intake, &intake_workspace, &registry, registry_text).unwrap();
        assert_eq!(labels, vec!["  Strip Gradient -> Function 11 (strip)"]);
        assert!(result.contains("<Function ID=\"11\" Type=\"RGBMatrix\" Name=\"Strip Gradient\">"));
        assert!(result.contains("<Color Index=\"0\">4294901760</Color>"));
        assert!(registry_result.contains("name = \"matrix:Strip Gradient\""));
        let mut parsed: Registry = toml::from_str(&registry_result).unwrap();
        let result_workspace = registry::parse_workspace(&result).unwrap();
        parsed.attach_periods(&result_workspace);
        assert!(parsed.validate(&result_workspace).is_empty());
        let (again, registry_again, _) = import_matrices(result.clone(), &intake, &intake_workspace, &parsed, &registry_result).unwrap();
        assert_eq!(again, result);
        assert_eq!(registry_again, registry_result);
        let original_intake_workspace = registry::parse_workspace(&production).unwrap();
        let (removed, registry_removed, _) = import_matrices(result, &production, &original_intake_workspace, &parsed, &registry_result).unwrap();
        assert_eq!(removed, production);
        assert_eq!(registry_removed, registry_text);
    }

    #[test]
    fn rejects_rgb_matrix_whose_group_layout_differs_from_production() {
        let production = "<Workspace><Engine>\n  <FixtureGroup ID=\"37\"><Name>Strip</Name></FixtureGroup>\n </Engine></Workspace>\n".to_owned();
        let intake = "<Workspace><Engine>\n  <FixtureGroup ID=\"37\"><Name>Reordered Strip</Name></FixtureGroup>\n  <Function ID=\"9\" Type=\"RGBMatrix\" Name=\"Gradient\"><FixtureGroup>37</FixtureGroup></Function>\n </Engine></Workspace>\n";
        let registry_text = "version = 1\nworkspace = \"prod.qxw\"\n[zones.strip]\nregions = [{ universe = 4, address = 15, channels = 3 }]\n";
        let registry: Registry = toml::from_str(registry_text).unwrap();
        let intake_workspace = registry::parse_workspace(intake).unwrap();
        assert!(import_matrices(production, intake, &intake_workspace, &registry, registry_text).unwrap_err().contains("differs from production"));
    }

    #[test]
    fn real_strip_group_accepts_an_animated_matrix_without_changing_existing_assets() {
        let root = crate::paths::config_dir();
        let production = std::fs::read_to_string(root.join("../qlcplus/monolith-lighting.qxw")).unwrap();
        let intake = std::fs::read_to_string(root.join("../qlcplus/intake/Monolithd Intake Template.qxw")).unwrap();
        let registry_text = std::fs::read_to_string(root.join("qlc-functions.toml")).unwrap();
        let registry: Registry = toml::from_str(&registry_text).unwrap();
        let matrix = concat!(
            "  <Function ID=\"900\" Type=\"RGBMatrix\" Name=\"Gradient Test\">\n",
            "   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"200\"/>\n",
            "   <Direction>Forward</Direction><RunOrder>Loop</RunOrder>\n",
            "   <Color Index=\"0\">4294901760</Color>\n",
            "   <FixtureGroup>37</FixtureGroup>\n",
            "  </Function>\n",
        );
        let intake = intake.replacen(" </Engine>", &format!("{matrix} </Engine>"), 1);
        let intake_workspace = registry::parse_workspace(&intake).unwrap();
        let (candidate, registry_candidate, labels) = import_matrices(production.clone(), &intake, &intake_workspace, &registry, &registry_text).unwrap();
        assert!(labels[0].contains("(strip)"));
        assert!(candidate.contains("Name=\"Gradient Test\""));
        assert!(candidate.contains("<FixtureGroup>37</FixtureGroup>"));
        let candidate_workspace = registry::parse_workspace(&candidate).unwrap();
        let mut candidate_registry: Registry = toml::from_str(&registry_candidate).unwrap();
        candidate_registry.attach_periods(&candidate_workspace);
        assert!(candidate_registry.validate(&candidate_workspace).is_empty());
        assert_eq!(candidate_workspace.functions.len(), registry::parse_workspace(&production).unwrap().functions.len() + 1);
    }
}
