use crate::config::Layout;
use quick_xml::events::{BytesStart, Event};
use quick_xml::XmlVersion;
use quick_xml::Reader;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const SUPPORTED_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
pub struct Registry {
    pub version: u32,
    pub workspace: String,
    pub zones: BTreeMap<String, ZoneGeometry>,
    #[serde(default)]
    pub functions: Vec<FunctionEntry>,
    #[serde(default)]
    pub progress: Vec<ProgressEntry>,
    #[serde(default)]
    pub ambient_sets: Vec<AmbientSet>,
    /// Cycle length of each chaser, by Function ID, read from the workspace when the registry is validated.
    #[serde(skip)]
    pub periods_ms: BTreeMap<u32, u32>,
}

#[derive(Debug, Deserialize)]
pub struct ZoneGeometry {
    pub regions: Vec<Region>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Region {
    pub universe: u32,
    pub address: u32,
    pub channels: u32,
}

#[derive(Debug, Deserialize)]
pub struct FunctionEntry {
    pub name: String,
    pub kind: String,
    pub id: u32,
    #[serde(default)]
    pub children: Vec<u32>,
    pub zones: Vec<String>,
    pub composable: bool,
}

#[derive(Debug, Deserialize)]
pub struct ProgressEntry {
    pub name: String,
    pub zone: String,
    pub label: String,
    pub first_id: u32,
    pub total: u32,
}

/// The zone functions that make up one ambient look, started together so they stay in phase.
#[derive(Debug, Deserialize)]
pub struct AmbientSet {
    pub name: String,
    pub functions: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Workspace {
    pub fixtures: BTreeMap<u32, Fixture>,
    pub functions: BTreeMap<u32, Function>,
}

#[derive(Debug, Clone, Copy)]
pub struct Fixture {
    pub universe: u32,
    pub address: u32,
    pub channels: u32,
}

#[derive(Debug, Default)]
pub struct Function {
    pub kind: String,
    pub name: String,
    /// Fixture ID -> fixture-relative channels this Function writes.
    pub writes: BTreeMap<u32, BTreeSet<u32>>,
    /// Child Function IDs in step order (chasers only).
    pub steps: Vec<u32>,
    /// The Function's `Speed Duration` attribute, in milliseconds.
    pub duration_ms: Option<u32>,
}

pub fn load(path: &Path) -> Result<Registry, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}

impl Registry {
    /// The workspace path, resolved relative to the registry file's directory.
    pub fn workspace_path(&self, registry_path: &Path) -> PathBuf {
        registry_path.parent().unwrap_or_else(|| Path::new(".")).join(&self.workspace)
    }

    pub fn function(&self, name: &str) -> Option<&FunctionEntry> {
        self.functions.iter().find(|entry| entry.name == name)
    }

    pub fn function_by_id(&self, id: u32) -> Option<&FunctionEntry> {
        self.functions.iter().find(|entry| entry.id == id)
    }

    pub fn progress_for_zone(&self, zone: &str) -> Option<&ProgressEntry> {
        self.progress.iter().find(|entry| entry.zone == zone)
    }

    /// A named progress family regardless of zone, for a job that asked for a
    /// specific one (e.g. an alternate RAM fill order) instead of its zone's default.
    pub fn progress_by_name(&self, name: &str) -> Option<&ProgressEntry> {
        self.progress.iter().find(|entry| entry.name == name)
    }

    pub fn ambient_set(&self, name: &str) -> Option<&AmbientSet> {
        self.ambient_sets.iter().find(|set| set.name == name)
    }

    /// The ambient set a named Function belongs to, if any.
    pub fn ambient_set_of(&self, function: &str) -> Option<&AmbientSet> {
        self.ambient_sets.iter().find(|set| set.functions.iter().any(|member| member == function))
    }

    /// Cycle length of a chaser in milliseconds (step count times its per-step duration).
    pub fn period_ms(&self, id: u32) -> Option<u32> {
        self.periods_ms.get(&id).copied()
    }

    /// Read each chaser's cycle length from the workspace. Chasers run their steps in
    /// turn for `Speed Duration` each, so a two-step chaser at 1800 ms cycles in 3.6 s.
    pub fn attach_periods(&mut self, workspace: &Workspace) {
        self.periods_ms.clear();
        for entry in self.functions.iter().filter(|entry| entry.kind.eq_ignore_ascii_case("chaser")) {
            let Some(function) = workspace.functions.get(&entry.id) else { continue };
            if let Some(duration) = function.duration_ms.filter(|duration| *duration > 0) {
                self.periods_ms.insert(entry.id, duration * function.steps.len() as u32);
            }
        }
    }

    fn zone_capacity(&self, zone: &str) -> Option<u32> {
        self.zones.get(zone).map(|geometry| geometry.regions.iter().map(|r| r.channels / 3).sum())
    }

    fn zone_of(&self, universe: u32, address: u32) -> Option<&str> {
        self.zones.iter().find_map(|(name, geometry)| {
            geometry
                .regions
                .iter()
                .any(|r| r.universe == universe && address >= r.address && address < r.address + r.channels)
                .then_some(name.as_str())
        })
    }

    /// Zones a Function actually writes, derived from its DMX channels (scenes)
    /// or the union of its children (chasers).
    fn resolved_zones(&self, workspace: &Workspace, id: u32, visiting: &mut Vec<u32>) -> Result<BTreeSet<String>, String> {
        if visiting.contains(&id) {
            return Err(format!("chaser cycle through function {id}"));
        }
        let function = workspace.functions.get(&id).ok_or_else(|| format!("function {id} is not in the workspace"))?;
        let mut zones = BTreeSet::new();
        match function.kind.as_str() {
            "Scene" => {
                for (fixture_id, channels) in &function.writes {
                    let fixture = workspace
                        .fixtures
                        .get(fixture_id)
                        .ok_or_else(|| format!("function {id} writes unknown fixture {fixture_id}"))?;
                    for channel in channels {
                        if *channel >= fixture.channels {
                            return Err(format!("function {id} writes channel {channel} beyond fixture {fixture_id}'s {} channels", fixture.channels));
                        }
                        let address = fixture.address + channel;
                        let zone = self.zone_of(fixture.universe, address).ok_or_else(|| {
                            format!("function {id} writes universe {} address {address} (fixture {fixture_id}), which belongs to no registered zone", fixture.universe)
                        })?;
                        zones.insert(zone.to_owned());
                    }
                }
            }
            "Chaser" => {
                visiting.push(id);
                for child in &function.steps {
                    zones.extend(self.resolved_zones(workspace, *child, visiting)?);
                }
                visiting.pop();
            }
            other => return Err(format!("function {id} has unsupported type {other}")),
        }
        Ok(zones)
    }

    /// Every disagreement between this registry and the workspace, not just the first.
    pub fn validate(&self, workspace: &Workspace) -> Vec<String> {
        let mut problems = Vec::new();
        if self.version != SUPPORTED_VERSION {
            problems.push(format!("registry version {} is unsupported (expected {SUPPORTED_VERSION})", self.version));
        }
        self.validate_geometry(&mut problems);

        let mut claimed: BTreeMap<u32, String> = BTreeMap::new();
        let mut claim = |id: u32, owner: String, problems: &mut Vec<String>| {
            if let Some(previous) = claimed.insert(id, owner.clone()) {
                problems.push(format!("function {id} is claimed by both {previous} and {owner}"));
            }
        };

        for entry in &self.functions {
            self.validate_function(entry, workspace, &mut problems);
            claim(entry.id, entry.name.clone(), &mut problems);
            for child in &entry.children {
                claim(*child, format!("{} (child)", entry.name), &mut problems);
            }
        }
        for entry in &self.progress {
            self.validate_progress(entry, workspace, &mut problems);
            for completed in 0..=entry.total {
                claim(entry.first_id + completed, format!("{} step {completed}", entry.name), &mut problems);
            }
        }
        self.validate_ambient_sets(&mut problems);
        problems
    }

    fn validate_ambient_sets(&self, problems: &mut Vec<String>) {
        let mut names = BTreeSet::new();
        let mut claimed: BTreeMap<&str, &str> = BTreeMap::new();
        for set in &self.ambient_sets {
            let label = format!("ambient set {}", set.name);
            if !names.insert(set.name.as_str()) {
                problems.push(format!("{label}: defined more than once"));
            }
            if set.functions.is_empty() {
                problems.push(format!("{label}: has no functions"));
            }
            let mut zones: BTreeSet<&str> = BTreeSet::new();
            let mut periods = BTreeSet::new();
            for name in &set.functions {
                let Some(entry) = self.function(name) else {
                    problems.push(format!("{label}: unknown function {name}"));
                    continue;
                };
                if !entry.kind.eq_ignore_ascii_case("chaser") {
                    problems.push(format!("{label}: {name} is a {}, ambient sets are made of chasers", entry.kind));
                }
                if !entry.composable || entry.zones.len() != 1 {
                    problems.push(format!("{label}: {name} must be composable and own exactly one zone"));
                }
                for zone in &entry.zones {
                    if !zones.insert(zone.as_str()) {
                        problems.push(format!("{label}: two functions drive zone {zone}"));
                    }
                }
                if let Some(period) = self.period_ms(entry.id) {
                    periods.insert(period);
                }
                if let Some(other) = claimed.insert(name.as_str(), set.name.as_str()) {
                    problems.push(format!("{label}: {name} is already in ambient set {other}"));
                }
            }
            if periods.len() > 1 {
                problems.push(format!("{label}: its chasers have different periods {periods:?} ms, so they cannot stay in phase"));
            }
        }
    }

    fn validate_geometry(&self, problems: &mut Vec<String>) {
        let regions: Vec<(&String, &Region)> = self.zones.iter().flat_map(|(name, g)| g.regions.iter().map(move |r| (name, r))).collect();
        for (index, (name_a, a)) in regions.iter().enumerate() {
            if a.channels == 0 || a.channels % 3 != 0 {
                problems.push(format!("zone {name_a} has a region of {} channels; RGB regions need a positive multiple of 3", a.channels));
            }
            for (name_b, b) in &regions[index + 1..] {
                if a.universe == b.universe && a.address < b.address + b.channels && b.address < a.address + a.channels {
                    problems.push(format!("zone regions overlap in universe {}: {name_a} and {name_b}", a.universe));
                }
            }
        }
    }

    fn validate_function(&self, entry: &FunctionEntry, workspace: &Workspace, problems: &mut Vec<String>) {
        let name = &entry.name;
        let Some(function) = workspace.functions.get(&entry.id) else {
            problems.push(format!("{name}: function {} is not in the workspace", entry.id));
            return;
        };
        if !function.kind.eq_ignore_ascii_case(&entry.kind) {
            problems.push(format!("{name}: function {} is a {} but the registry says {}", entry.id, function.kind, entry.kind));
        }
        if function.steps != entry.children {
            problems.push(format!("{name}: function {} has children {:?} but the registry lists {:?}", entry.id, function.steps, entry.children));
        }
        for zone in &entry.zones {
            if !self.zones.contains_key(zone) {
                problems.push(format!("{name}: declares unknown zone {zone}"));
            }
        }
        if entry.composable && entry.zones.len() != 1 {
            problems.push(format!("{name}: composable functions must own exactly one zone, not {}", entry.zones.len()));
        }
        match self.resolved_zones(workspace, entry.id, &mut Vec::new()) {
            Ok(actual) => {
                let declared: BTreeSet<String> = entry.zones.iter().cloned().collect();
                if actual != declared {
                    problems.push(format!("{name}: function {} writes zones {actual:?} but the registry declares {declared:?}", entry.id));
                }
            }
            Err(error) => problems.push(format!("{name}: {error}")),
        }
    }

    fn validate_progress(&self, entry: &ProgressEntry, workspace: &Workspace, problems: &mut Vec<String>) {
        let name = &entry.name;
        match self.zone_capacity(&entry.zone) {
            None => {
                problems.push(format!("{name}: unknown zone {}", entry.zone));
                return;
            }
            Some(capacity) if capacity != entry.total => {
                problems.push(format!("{name}: total is {} but zone {} has {capacity} RGB LEDs", entry.total, entry.zone));
            }
            Some(_) => {}
        }
        let expected_zone: BTreeSet<String> = BTreeSet::from([entry.zone.clone()]);
        for completed in 0..=entry.total {
            let id = entry.first_id + completed;
            let Some(function) = workspace.functions.get(&id) else {
                problems.push(format!("{name}: step {completed} (function {id}) is not in the workspace"));
                continue;
            };
            let expected_name = format!("Progress {} {completed:02} of {}", entry.label, entry.total);
            if function.name != expected_name {
                problems.push(format!("{name}: function {id} is named {:?}, expected {expected_name:?}", function.name));
            }
            if function.kind != "Scene" {
                problems.push(format!("{name}: function {id} is a {}, progress steps must be Scenes", function.kind));
            }
            match self.resolved_zones(workspace, id, &mut Vec::new()) {
                Ok(actual) if actual == expected_zone => {}
                Ok(actual) => problems.push(format!("{name}: function {id} writes zones {actual:?}, expected only {}", entry.zone)),
                Err(error) => problems.push(format!("{name}: {error}")),
            }
        }
    }

    /// Cross-check zone sizes against scene-layout.toml where it states an LED count.
    pub fn validate_layout(&self, layout: &Layout) -> Vec<String> {
        let mut problems = Vec::new();
        for (name, zone) in &layout.zones {
            if let (Some(leds), Some(capacity)) = (zone.led_count, self.zone_capacity(name)) {
                if leds as u32 != capacity {
                    problems.push(format!("zone {name}: scene-layout.toml says {leds} LEDs but the registry geometry holds {capacity}"));
                }
            }
        }
        problems
    }
}

#[derive(Default)]
struct PendingFixture {
    id: Option<u32>,
    universe: Option<u32>,
    address: Option<u32>,
    channels: Option<u32>,
}

#[derive(Default)]
struct Reading {
    workspace: Workspace,
    fixture: PendingFixture,
    function: Option<(u32, Function)>,
    /// (step number, child function ID) for the chaser being read.
    steps: Vec<(u32, u32)>,
    step_number: Option<u32>,
    writing_fixture: Option<u32>,
}

fn attribute(element: &BytesStart, key: &str) -> Result<Option<String>, String> {
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|e| format!("bad attribute: {e}"))?;
        if attribute.key.as_ref() == key {
            let value = attribute.normalized_value(XmlVersion::Explicit1_0).map_err(|e| format!("bad {key} value: {e}"))?;
            return Ok(Some(value.into_owned()));
        }
    }
    Ok(None)
}

fn number(text: &str, what: &str) -> Result<u32, String> {
    text.trim().parse().map_err(|_| format!("{what} {text:?} is not an integer"))
}

fn at(path: &[String], expected: &[&str]) -> bool {
    path.len() == expected.len() && path.iter().zip(expected).all(|(a, b)| a == b)
}

impl Reading {
    /// `path` is the element stack *excluding* the element being opened.
    fn start(&mut self, path: &[String], name: &str, element: &BytesStart) -> Result<(), String> {
        if at(path, &["Workspace", "Engine"]) && name == "Function" {
            let id = number(&attribute(element, "ID")?.ok_or("Function without ID")?, "Function ID")?;
            let function = Function {
                kind: attribute(element, "Type")?.unwrap_or_default(),
                name: attribute(element, "Name")?.unwrap_or_default(),
                ..Function::default()
            };
            self.function = Some((id, function));
            self.steps.clear();
        } else if at(path, &["Workspace", "Engine"]) && name == "Fixture" {
            self.fixture = PendingFixture::default();
        } else if at(path, &["Workspace", "Engine", "Function"]) && name == "FixtureVal" {
            self.writing_fixture = Some(number(&attribute(element, "ID")?.ok_or("FixtureVal without ID")?, "FixtureVal ID")?);
        } else if at(path, &["Workspace", "Engine", "Function"]) && name == "Step" {
            self.step_number = attribute(element, "Number")?.map(|n| number(&n, "Step Number")).transpose()?;
        } else if at(path, &["Workspace", "Engine", "Function"]) && name == "Speed" {
            if let Some((_, function)) = self.function.as_mut() {
                function.duration_ms = attribute(element, "Duration")?.map(|value| number(&value, "Speed Duration")).transpose()?;
            }
        }
        Ok(())
    }

    /// `path` is the element stack *including* the element that holds the text.
    fn text(&mut self, path: &[String], text: &str) -> Result<(), String> {
        if path.len() == 4 && at(&path[..3], &["Workspace", "Engine", "Fixture"]) {
            match path[3].as_str() {
                "ID" => self.fixture.id = Some(number(text, "Fixture ID")?),
                "Universe" => self.fixture.universe = Some(number(text, "Fixture Universe")?),
                "Address" => self.fixture.address = Some(number(text, "Fixture Address")?),
                "Channels" => self.fixture.channels = Some(number(text, "Fixture Channels")?),
                _ => {}
            }
        } else if at(path, &["Workspace", "Engine", "Function", "FixtureVal"]) {
            let (Some(fixture), Some((_, function))) = (self.writing_fixture, self.function.as_mut()) else { return Ok(()) };
            let values: Vec<&str> = text.trim().split(',').collect();
            if values.len() % 2 != 0 {
                return Err(format!("FixtureVal for fixture {fixture} has an odd number of entries"));
            }
            let channels = function.writes.entry(fixture).or_default();
            for pair in values.chunks(2) {
                channels.insert(number(pair[0], "FixtureVal channel")?);
            }
        } else if at(path, &["Workspace", "Engine", "Function", "Step"]) {
            let child = number(text, "Step function ID")?;
            let position = self.step_number.unwrap_or(self.steps.len() as u32);
            self.steps.push((position, child));
        }
        Ok(())
    }

    /// `path` is the element stack *including* the element being closed.
    fn end(&mut self, path: &[String]) -> Result<(), String> {
        if at(path, &["Workspace", "Engine", "Fixture"]) {
            let f = std::mem::take(&mut self.fixture);
            let (Some(id), Some(universe), Some(address), Some(channels)) = (f.id, f.universe, f.address, f.channels) else {
                return Err("Fixture is missing ID, Universe, Address, or Channels".to_owned());
            };
            if self.workspace.fixtures.insert(id, Fixture { universe, address, channels }).is_some() {
                return Err(format!("duplicate fixture ID {id}"));
            }
        } else if at(path, &["Workspace", "Engine", "Function"]) {
            if let Some((id, mut function)) = self.function.take() {
                self.steps.sort();
                function.steps = self.steps.drain(..).map(|(_, child)| child).collect();
                if self.workspace.functions.insert(id, function).is_some() {
                    return Err(format!("duplicate function ID {id}"));
                }
            }
        } else if at(path, &["Workspace", "Engine", "Function", "FixtureVal"]) {
            self.writing_fixture = None;
        } else if at(path, &["Workspace", "Engine", "Function", "Step"]) {
            self.step_number = None;
        }
        Ok(())
    }
}

pub fn parse_workspace(xml: &str) -> Result<Workspace, String> {
    let mut reader = Reader::from_str(xml);
    let mut state = Reading::default();
    let mut path: Vec<String> = Vec::new();
    loop {
        let event = reader.read_event().map_err(|e| format!("workspace XML error near byte {}: {e}", reader.buffer_position()))?;
        match event {
            Event::Start(element) => {
                let name = element.local_name().as_ref().to_owned();
                state.start(&path, &name, &element)?;
                path.push(name);
            }
            Event::Empty(element) => {
                let name = element.local_name().as_ref().to_owned();
                state.start(&path, &name, &element)?;
                path.push(name);
                state.end(&path)?;
                path.pop();
            }
            Event::Text(text) => {
                state.text(&path, &text.xml10_content())?;
            }
            Event::End(_) => {
                state.end(&path)?;
                path.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(state.workspace)
}

/// Load the registry and check it against the workspace and scene-layout.toml.
/// Returns the registry together with every problem found (empty means trustworthy).
pub fn load_and_validate(registry_path: &Path, layout: &Layout) -> Result<(Registry, Vec<String>), String> {
    let mut registry = load(registry_path)?;
    let workspace_path = registry.workspace_path(registry_path);
    let xml = std::fs::read_to_string(&workspace_path).map_err(|e| format!("read {}: {e}", workspace_path.display()))?;
    let workspace = parse_workspace(&xml).map_err(|e| format!("parse {}: {e}", workspace_path.display()))?;
    registry.attach_periods(&workspace);
    let mut problems = registry.validate(&workspace);
    problems.extend(registry.validate_layout(layout));
    Ok((registry, problems))
}

/// Full offline validation for `monolithd validate-registry`.
pub fn validate_files(registry_path: &Path, layout: &Layout) -> Result<Vec<String>, String> {
    let (registry, problems) = load_and_validate(registry_path, layout)?;
    println!(
        "registry v{}: {} functions, {} progress families checked against {}",
        registry.version,
        registry.functions.len(),
        registry.progress.len(),
        registry.workspace_path(registry_path).display(),
    );
    Ok(problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf() }

    const REGISTRY: &str = r#"
        version = 1
        workspace = "ws.qxw"
        [zones.left]
        regions = [{ universe = 0, address = 0, channels = 6 }]
        [zones.right]
        regions = [{ universe = 0, address = 6, channels = 6 }]
        [[functions]]
        name = "ambient_left"
        kind = "chaser"
        id = 3
        children = [1, 2]
        zones = ["left"]
        composable = true
        [[progress]]
        name = "progress_right"
        zone = "right"
        label = "Right"
        first_id = 10
        total = 2
    "#;

    fn workspace_xml(mutate: impl Fn(String) -> String) -> String {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE Workspace>
<Workspace xmlns="http://www.qlcplus.org/Workspace">
 <Engine>
  <Fixture>
   <Manufacturer>Generic</Manufacturer>
   <ID>0</ID>
   <Name>Left &amp; Right</Name>
   <Universe>0</Universe>
   <Address>0</Address>
   <Channels>12</Channels>
  </Fixture>
  <Function ID="1" Type="Scene" Name="Low">
   <Speed FadeIn="0" FadeOut="0" Duration="0"/>
   <FixtureVal ID="0">0,10,1,0,2,255,3,10,4,0,5,255</FixtureVal>
  </Function>
  <Function ID="2" Type="Scene" Name="High">
   <FixtureVal ID="0">0,90,1,0,2,255,3,90,4,0,5,255</FixtureVal>
  </Function>
  <Function ID="3" Type="Chaser" Name="Pulse">
   <Step Number="1">2</Step>
   <Step Number="0">1</Step>
  </Function>
  <Function ID="10" Type="Scene" Name="Progress Right 00 of 2">
   <FixtureVal ID="0">6,0,7,0,8,0,9,0,10,0,11,0</FixtureVal>
  </Function>
  <Function ID="11" Type="Scene" Name="Progress Right 01 of 2">
   <FixtureVal ID="0">6,255,7,255,8,255,9,0,10,0,11,0</FixtureVal>
  </Function>
  <Function ID="12" Type="Scene" Name="Progress Right 02 of 2">
   <FixtureVal ID="0">6,255,7,255,8,255,9,255,10,255,11,255</FixtureVal>
  </Function>
 </Engine>
</Workspace>
"#;
        mutate(xml.to_owned())
    }

    fn check(mutate: impl Fn(String) -> String) -> Vec<String> {
        let registry: Registry = toml::from_str(REGISTRY).unwrap();
        registry.validate(&parse_workspace(&workspace_xml(mutate)).unwrap())
    }

    #[test]
    fn parses_fixtures_functions_and_ordered_steps() {
        let workspace = parse_workspace(&workspace_xml(|xml| xml)).unwrap();
        assert_eq!(workspace.fixtures.len(), 1);
        assert_eq!(workspace.functions.len(), 6);
        // Steps are ordered by their Number attribute, not document order.
        assert_eq!(workspace.functions[&3].steps, vec![1, 2]);
        assert_eq!(workspace.functions[&1].writes[&0].len(), 6);
    }

    #[test]
    fn accepts_a_consistent_registry() {
        assert_eq!(check(|xml| xml), Vec::<String>::new());
    }

    #[test]
    fn rejects_a_missing_function() {
        let problems = check(|xml| xml.replace("ID=\"12\"", "ID=\"99\""));
        assert!(problems.iter().any(|p| p.contains("step 2 (function 12) is not in the workspace")), "{problems:?}");
    }

    #[test]
    fn rejects_a_wrong_type() {
        let problems = check(|xml| xml.replace("Type=\"Chaser\"", "Type=\"Scene\""));
        assert!(problems.iter().any(|p| p.contains("is a Scene but the registry says chaser")), "{problems:?}");
    }

    #[test]
    fn rejects_mismatched_chaser_children() {
        let problems = check(|xml| xml.replace("<Step Number=\"1\">2</Step>", "<Step Number=\"1\">1</Step>"));
        assert!(problems.iter().any(|p| p.contains("has children [1, 1]")), "{problems:?}");
    }

    #[test]
    fn rejects_a_scene_that_leaks_into_another_zone() {
        // High also writes channel 6 (the right zone).
        let problems = check(|xml| xml.replace("3,90,4,0,5,255", "3,90,4,0,5,255,6,1"));
        assert!(problems.iter().any(|p| p.contains("writes zones") && p.contains("right")), "{problems:?}");
    }

    #[test]
    fn rejects_a_misnamed_progress_step() {
        let problems = check(|xml| xml.replace("Progress Right 01 of 2", "Progress Right 1 of 2"));
        assert!(problems.iter().any(|p| p.contains("expected \"Progress Right 01 of 2\"")), "{problems:?}");
    }

    #[test]
    fn rejects_a_progress_total_that_disagrees_with_the_zone() {
        let registry: Registry = toml::from_str(&REGISTRY.replace("total = 2", "total = 3")).unwrap();
        let problems = registry.validate(&parse_workspace(&workspace_xml(|xml| xml)).unwrap());
        assert!(problems.iter().any(|p| p.contains("total is 3 but zone right has 2 RGB LEDs")), "{problems:?}");
    }

    #[test]
    fn rejects_overlapping_id_claims() {
        let registry: Registry = toml::from_str(&REGISTRY.replace("first_id = 10", "first_id = 2")).unwrap();
        let problems = registry.validate(&parse_workspace(&workspace_xml(|xml| xml)).unwrap());
        assert!(problems.iter().any(|p| p.contains("function 2 is claimed by both")), "{problems:?}");
    }

    #[test]
    fn rejects_composable_multi_zone_functions() {
        let registry: Registry = toml::from_str(&REGISTRY.replace("zones = [\"left\"]", "zones = [\"left\", \"right\"]")).unwrap();
        let problems = registry.validate(&parse_workspace(&workspace_xml(|xml| xml)).unwrap());
        assert!(problems.iter().any(|p| p.contains("composable functions must own exactly one zone")), "{problems:?}");
    }

    #[test]
    fn rejects_malformed_xml() {
        assert!(parse_workspace("<Workspace><Engine><Fixture></Engine>").is_err());
    }

    #[test]
    fn the_real_registry_matches_the_real_workspace() {
        let root = root();
        let layout = crate::config::load_layout(&root.join("scene-layout.toml")).unwrap();
        let problems = validate_files(&root.join("qlc-functions.toml"), &layout).unwrap();
        assert_eq!(problems, Vec::<String>::new());
    }

    #[test]
    fn chaser_periods_come_from_the_workspace_speed_and_step_count() {
        let xml = workspace_xml(|xml| {
            xml.replace(
                "<Function ID=\"3\" Type=\"Chaser\" Name=\"Pulse\">\n",
                "<Function ID=\"3\" Type=\"Chaser\" Name=\"Pulse\">\n   <Speed FadeIn=\"10\" FadeOut=\"10\" Duration=\"250\"/>\n",
            )
        });
        let mut registry: Registry = toml::from_str(REGISTRY).unwrap();
        registry.attach_periods(&parse_workspace(&xml).unwrap());
        assert_eq!(registry.period_ms(3), Some(500), "two steps of 250 ms");
        assert_eq!(registry.period_ms(10), None, "progress scenes have no period");
    }

    #[test]
    fn the_real_ambient_set_has_one_shared_period() {
        let root = root();
        let layout = crate::config::load_layout(&root.join("scene-layout.toml")).unwrap();
        let (registry, problems) = load_and_validate(&root.join("qlc-functions.toml"), &layout).unwrap();
        assert_eq!(problems, Vec::<String>::new());
        assert_eq!([109, 112, 115].map(|id| registry.period_ms(id)), [Some(3600); 3]);
        assert_eq!(registry.ambient_set("deep_violet").unwrap().functions.len(), 3);
        assert_eq!(registry.ambient_set_of("ambient_eye").unwrap().name, "deep_violet");
        assert!(registry.ambient_set_of("boot_proof").is_none());
    }

    fn set_problems(extra: &str) -> Vec<String> {
        let registry: Registry = toml::from_str(&format!("{REGISTRY}\n{extra}")).unwrap();
        let mut problems = Vec::new();
        registry.validate_ambient_sets(&mut problems);
        problems
    }

    #[test]
    fn ambient_sets_are_validated() {
        assert_eq!(set_problems("[[ambient_sets]]\nname = \"s\"\nfunctions = [\"ambient_left\"]\n"), Vec::<String>::new());
        assert!(set_problems("[[ambient_sets]]\nname = \"s\"\nfunctions = [\"nope\"]\n")[0].contains("unknown function nope"));
        assert!(set_problems("[[ambient_sets]]\nname = \"s\"\nfunctions = []\n")[0].contains("has no functions"));
        let twice = set_problems(
            "[[ambient_sets]]\nname = \"s\"\nfunctions = [\"ambient_left\"]\n[[ambient_sets]]\nname = \"s\"\nfunctions = [\"ambient_left\"]\n",
        );
        assert!(twice.iter().any(|p| p.contains("defined more than once")), "{twice:?}");
        assert!(twice.iter().any(|p| p.contains("already in ambient set")), "{twice:?}");
    }

    #[test]
    fn chasers_with_different_periods_cannot_share_a_set() {
        let text = format!(
            "{REGISTRY}\n[[functions]]\nname = \"ambient_right\"\nkind = \"chaser\"\nid = 5\nchildren = [1, 2]\nzones = [\"right\"]\ncomposable = true\n[[ambient_sets]]\nname = \"s\"\nfunctions = [\"ambient_left\", \"ambient_right\"]\n"
        );
        let mut registry: Registry = toml::from_str(&text).unwrap();
        registry.periods_ms.insert(3, 3600);
        registry.periods_ms.insert(5, 3000);
        let mut problems = Vec::new();
        registry.validate_ambient_sets(&mut problems);
        assert!(problems.iter().any(|p| p.contains("different periods")), "{problems:?}");
    }
}
