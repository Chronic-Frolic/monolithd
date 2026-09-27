//! Animated progress assets: per-LED loops and the Cue Lists that start Chasers at a step.
//!
//! Owner decisions 2026-09-26: a progress bar keeps a truthful, discrete fill boundary
//! while every LED plays its own loop, a Full loop on the lit side and an Empty loop on the
//! other; the Full loop belongs to the base look, and the Empty side is either the base
//! look itself or a distinct working look. QLC+ blends Functions sharing a channel by
//! per-channel maximum (measured 2026-09-26), so a per-LED loop cannot override the zone's
//! ambient on one LED: while a bar shows, the ambient Chaser stops and per-LED loops
//! replace it everywhere in the zone. QLC+ cannot move a running Chaser to another step,
//! but a stopped Cue List plays its Chaser from the step selected on it (probed
//! 2026-09-26), so every loop has its own Cue List and starts at the step the others are on.
//!
//! The looks themselves are ordinary registered whole-zone Chasers authored in QLC+
//! (through the intake like every other look): `tools/slice-progress-loops.py` slices each
//! into one single-LED loop per fixture, lists them in `config/progress-animation.toml`, and
//! `validate-registry` checks the slices still match their source, so an edited look whose
//! loops were not re-sliced is caught rather than shown half-old.

use crate::paths;
use crate::registry::Registry;
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};

/// The Empty side's look when the bar is a distinct working state rather than the base look.
pub const WORKING: &str = "working";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Full,
    Empty,
}

/// A Cue List that starts a registered Function (a zone's ambient Chaser) at a step.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CueEntry {
    pub function: String,
    pub cue_list: u32,
}

/// One kind of loop for every LED of a zone: parallel lists, one entry per fixture.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopSet {
    pub zone: String,
    pub role: Role,
    /// The base look (ambient set) this loop belongs to, or `working`.
    pub look: String,
    /// The registered whole-zone Chaser these loops are slices of.
    pub source: String,
    pub step_ms: u32,
    pub steps: u32,
    pub fixtures: Vec<u32>,
    pub chasers: Vec<u32>,
    pub cue_lists: Vec<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Animation {
    pub version: u32,
    #[serde(default)]
    pub cues: Vec<CueEntry>,
    #[serde(default)]
    pub loops: Vec<LoopSet>,
}

impl Animation {
    pub fn parse(text: &str) -> Result<Self, String> {
        let animation: Self = toml::from_str(text).map_err(|error| error.to_string())?;
        if animation.version != 1 {
            return Err(format!("unsupported version {}", animation.version));
        }
        for set in &animation.loops {
            let n = set.fixtures.len();
            if set.chasers.len() != n || set.cue_lists.len() != n {
                return Err(format!("loops {}/{:?}/{}: fixtures, chasers and cue_lists differ in length", set.zone, set.role, set.look));
            }
            if set.steps == 0 || set.step_ms == 0 {
                return Err(format!("loops {}/{:?}/{}: steps and step_ms must be positive", set.zone, set.role, set.look));
            }
        }
        Ok(animation)
    }

    /// `config/progress-animation.toml`; `None` when there is none (no animated bars).
    pub fn load() -> Result<Option<Self>, String> {
        let path = paths::config_dir().join("progress-animation.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text).map(Some).map_err(|error| format!("{}: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("read {}: {error}", path.display())),
        }
    }

    pub fn cue_for(&self, function: &str) -> Option<u32> {
        self.cues.iter().find(|entry| entry.function == function).map(|entry| entry.cue_list)
    }

    pub fn loops(&self, zone: &str, role: Role, look: &str) -> Option<&LoopSet> {
        self.loops.iter().find(|set| set.zone == zone && set.role == role && set.look == look)
    }

    /// Every per-LED loop Chaser of a zone, of every look (for cleaning up after trouble).
    pub fn zone_chasers(&self, zone: &str) -> Vec<u32> {
        self.loops.iter().filter(|set| set.zone == zone).flat_map(|set| set.chasers.iter().copied()).collect()
    }

    /// Every disagreement with the registry and the workspace XML.
    pub fn check(&self, xml: &str, registry: &Registry) -> Vec<String> {
        let xml = &Index::new(xml);
        let mut problems = Vec::new();
        let mut cue_ids = BTreeSet::new();
        for entry in &self.cues {
            match registry.function(&entry.function) {
                None => problems.push(format!("cue list {}: {} is not a registered function", entry.cue_list, entry.function)),
                Some(function) => check_cue(xml, entry.cue_list, function.id, &mut problems),
            }
            if !cue_ids.insert(entry.cue_list) {
                problems.push(format!("cue list {} is used twice", entry.cue_list));
            }
        }
        for set in &self.loops {
            let label = format!("loops {}/{:?}/{}", set.zone, set.role, set.look);
            if !registry.zones.contains_key(&set.zone) {
                problems.push(format!("{label}: unknown zone {}", set.zone));
            }
            if set.role == Role::Full && set.look == WORKING {
                problems.push(format!("{label}: the working look is an Empty side only"));
            }
            if set.look != WORKING && registry.ambient_set(&set.look).is_none() {
                problems.push(format!("{label}: {} is not an ambient set", set.look));
            }
            let source = match registry.function(&set.source) {
                None => {
                    problems.push(format!("{label}: source {} is not a registered function", set.source));
                    None
                }
                Some(entry) if entry.kind != "chaser" || entry.zones != [set.zone.clone()] => {
                    problems.push(format!("{label}: source {} must be a chaser on zone {} alone", set.source, set.zone));
                    None
                }
                Some(entry) => {
                    if chaser_steps(xml, entry.id).is_some_and(|(steps, duration)| steps != set.steps as usize || duration != set.step_ms) {
                        problems.push(format!("{label}: source {} no longer has {} steps of {} ms; re-slice the loops", set.source, set.steps, set.step_ms));
                    }
                    Some(entry)
                }
            };
            let mut stale = 0;
            for ((&fixture, &chaser), &cue_list) in set.fixtures.iter().zip(&set.chasers).zip(&set.cue_lists) {
                match chaser_steps(xml, chaser) {
                    None => problems.push(format!("{label}: Chaser {chaser} (fixture {fixture}) is not in the workspace")),
                    Some((steps, duration)) => {
                        if steps != set.steps as usize {
                            problems.push(format!("{label}: Chaser {chaser} has {steps} steps, not {}", set.steps));
                        }
                        if duration != set.step_ms {
                            problems.push(format!("{label}: Chaser {chaser} steps last {duration} ms, not {}", set.step_ms));
                        }
                    }
                }
                if let Some(source) = source {
                    let slices = chaser_children(xml, chaser);
                    let matches = slices.len() == source.children.len()
                        && slices.iter().zip(&source.children).all(|(&slice, &whole)| {
                            let slice_block = function_block(xml, slice);
                            let one = slice_block.map(|block| block.matches("<FixtureVal ").count()) == Some(1);
                            one && slice_block.and_then(|block| fixture_val(block, fixture)).is_some_and(|value| function_block(xml, whole).and_then(|block| fixture_val(block, fixture)) == Some(value))
                        });
                    if !matches {
                        stale += 1;
                    }
                }
                check_cue(xml, cue_list, chaser, &mut problems);
                if !cue_ids.insert(cue_list) {
                    problems.push(format!("cue list {cue_list} is used twice"));
                }
            }
            if stale > 0 {
                problems.push(format!("{label}: {stale} of {} loops no longer match {}; re-run tools/slice-progress-loops.py", set.fixtures.len(), set.source));
            }
        }
        problems
    }
}

/// The workspace's Function and Cue List blocks by ID, found in one pass: the checks look
/// up thousands of them in a 1.4 MB file.
struct Index<'a> {
    functions: HashMap<u32, &'a str>,
    cues: HashMap<u32, &'a str>,
}

impl<'a> Index<'a> {
    fn new(xml: &'a str) -> Self {
        let mut functions = HashMap::new();
        for (start, tag) in xml.match_indices("<Function ID=\"") {
            let Some(id) = xml[start + tag.len()..].split('"').next().and_then(|id| id.parse().ok()) else { continue };
            if let Some(length) = xml[start..].find("</Function>") {
                // The Engine's definitions come first; later mentions (Virtual Console) do not count.
                functions.entry(id).or_insert(&xml[start..start + length]);
            }
        }
        let mut cues = HashMap::new();
        for (start, _) in xml.match_indices("<CueList ") {
            let tag = xml[start..].split('>').next().unwrap_or("");
            let Some(id) = tag.split(" ID=\"").nth(1).and_then(|rest| rest.split('"').next()).and_then(|id| id.parse().ok()) else { continue };
            if let Some(length) = xml[start..].find("</CueList>") {
                cues.entry(id).or_insert(&xml[start..start + length]);
            }
        }
        Self { functions, cues }
    }
}

/// The Function block with this ID, if any.
fn function_block<'a>(xml: &Index<'a>, id: u32) -> Option<&'a str> {
    xml.functions.get(&id).copied()
}

/// A Chaser's step count and step Duration (ms).
fn chaser_steps(xml: &Index, id: u32) -> Option<(usize, u32)> {
    let block = function_block(xml, id)?;
    if !block.contains("Type=\"Chaser\"") {
        return None;
    }
    let speed = &block[block.find("<Speed ")?..];
    let duration = speed[speed.find("Duration=\"")? + 10..].split('"').next()?.parse().ok()?;
    Some((block.matches("<Step ").count(), duration))
}

/// A Chaser's step Function IDs, in order.
fn chaser_children(xml: &Index, id: u32) -> Vec<u32> {
    let Some(block) = function_block(xml, id) else { return Vec::new() };
    block.split("<Step ").skip(1).filter_map(|step| step.split_once('>')?.1.split('<').next()?.trim().parse().ok()).collect()
}

/// A Scene's value list for one fixture.
fn fixture_val(block: &str, fixture: u32) -> Option<&str> {
    let tag = format!("<FixtureVal ID=\"{fixture}\">");
    let start = block.find(&tag)? + tag.len();
    Some(&block[start..start + block[start..].find('<')?])
}

fn check_cue(xml: &Index, cue_list: u32, function: u32, problems: &mut Vec<String>) {
    match xml.cues.get(&cue_list) {
        None => problems.push(format!("cue list {cue_list} is not in the Virtual Console")),
        Some(block) if !block.contains(&format!("<Chaser>{function}</Chaser>")) => problems.push(format!("cue list {cue_list} does not drive Function {function}")),
        Some(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = concat!(
        "  <Function ID=\"500\" Type=\"Chaser\" Name=\"L0\">\n   <Speed FadeIn=\"1250\" FadeOut=\"1250\" Duration=\"1250\"/>\n   <Step Number=\"0\">1</Step>\n   <Step Number=\"1\">2</Step>\n  </Function>\n",
        "  <Function ID=\"1\" Type=\"Scene\" Name=\"S1\">\n   <FixtureVal ID=\"0\">0,1,1,2,2,3</FixtureVal>\n  </Function>\n",
        "  <Function ID=\"2\" Type=\"Scene\" Name=\"S2\">\n   <FixtureVal ID=\"0\">0,4,1,5,2,6</FixtureVal>\n  </Function>\n",
        "  <Function ID=\"600\" Type=\"Chaser\" Name=\"Aurora\">\n   <Speed FadeIn=\"1250\" FadeOut=\"1250\" Duration=\"1250\"/>\n   <Step Number=\"0\">601</Step>\n   <Step Number=\"1\">602</Step>\n  </Function>\n",
        "  <Function ID=\"601\" Type=\"Scene\" Name=\"A1\">\n   <FixtureVal ID=\"0\">0,1,1,2,2,3</FixtureVal>\n   <FixtureVal ID=\"1\">0,9,1,9,2,9</FixtureVal>\n  </Function>\n",
        "  <Function ID=\"602\" Type=\"Scene\" Name=\"A2\">\n   <FixtureVal ID=\"0\">0,4,1,5,2,6</FixtureVal>\n   <FixtureVal ID=\"1\">0,9,1,9,2,9</FixtureVal>\n  </Function>\n",
        "  <Function ID=\"109\" Type=\"Chaser\" Name=\"A\">\n   <Speed FadeIn=\"0\" FadeOut=\"0\" Duration=\"1250\"/>\n   <Step Number=\"0\">1</Step>\n  </Function>\n",
        "   <CueList Caption=\"C\" ID=\"9000\">\n    <Chaser>109</Chaser>\n   </CueList>\n",
        "   <CueList Caption=\"C\" ID=\"10000\">\n    <Chaser>500</Chaser>\n   </CueList>\n",
    );

    fn animation(extra: &str) -> Animation {
        Animation::parse(&format!(
            "version = 1\n[[cues]]\nfunction = \"ambient_ram\"\ncue_list = 9000\n[[loops]]\nzone = \"ram\"\nrole = \"full\"\nlook = \"deep_violet\"\nsource = \"aurora_ram\"\nstep_ms = 1250\nsteps = 2\nfixtures = [0]\nchasers = [500]\ncue_lists = [10000]\n{extra}"
        ))
        .unwrap()
    }

    fn registry() -> Registry {
        toml::from_str("version = 1\nworkspace = \"x\"\n[zones.ram]\nregions = []\n[[functions]]\nname = \"ambient_ram\"\nkind = \"chaser\"\nid = 109\nchildren = [1]\nzones = [\"ram\"]\ncomposable = true\n[[functions]]\nname = \"aurora_ram\"\nkind = \"chaser\"\nid = 600\nchildren = [601, 602]\nzones = [\"ram\"]\ncomposable = true\n[[ambient_sets]]\nname = \"deep_violet\"\nfunctions = [\"ambient_ram\"]\n").unwrap()
    }

    #[test]
    fn a_consistent_animation_checks_out_and_answers_lookups() {
        let animation = animation("");
        assert_eq!(animation.check(XML, &registry()), Vec::<String>::new());
        assert_eq!(animation.cue_for("ambient_ram"), Some(9000));
        assert!(animation.loops("ram", Role::Full, "deep_violet").is_some());
        assert!(animation.loops("ram", Role::Empty, "deep_violet").is_none());
        assert_eq!(animation.zone_chasers("ram"), vec![500]);
    }

    #[test]
    fn the_shipped_loops_match_the_looks_they_were_sliced_from() {
        let path = paths::config_dir().join("qlc-functions.toml");
        let registry = crate::registry::load(&path).unwrap();
        let animation = Animation::load().unwrap().expect("config/progress-animation.toml is shipped");
        let xml = std::fs::read_to_string(registry.workspace_path(&path)).unwrap();
        assert_eq!(animation.check(&xml, &registry), Vec::<String>::new(), "edited a look? re-run tools/slice-progress-loops.py");
    }

    #[test]
    fn mistakes_are_reported() {
        let wrong_steps = Animation { loops: vec![LoopSet { steps: 3, ..animation("").loops[0].clone() }], ..animation("") };
        assert!(wrong_steps.check(XML, &registry()).iter().any(|problem| problem.contains("has 2 steps, not 3")));
        let missing = Animation { loops: vec![LoopSet { chasers: vec![501], ..animation("").loops[0].clone() }], ..animation("") };
        let problems = missing.check(XML, &registry());
        assert!(problems.iter().any(|problem| problem.contains("Chaser 501 (fixture 0) is not in the workspace")), "{problems:?}");
        assert!(problems.iter().any(|problem| problem.contains("does not drive Function 501")), "{problems:?}");
        let edited = XML.replace("0,4,1,5,2,6</FixtureVal>\n   <FixtureVal ID=\"1\">", "0,4,1,5,2,7</FixtureVal>\n   <FixtureVal ID=\"1\">");
        let problems = animation("").check(&edited, &registry());
        assert!(problems.iter().any(|problem| problem.contains("1 of 1 loops no longer match aurora_ram")), "an edited look must be re-sliced: {problems:?}");
        let wrong_source = Animation { loops: vec![LoopSet { source: "ambient_ram".to_owned(), ..animation("").loops[0].clone() }], ..animation("") };
        assert!(wrong_source.check(XML, &registry()).iter().any(|problem| problem.contains("no longer match ambient_ram")));
        assert!(Animation::parse("version = 1\n[[loops]]\nzone = \"ram\"\nrole = \"full\"\nlook = \"x\"\nsource = \"y\"\nstep_ms = 1\nsteps = 1\nfixtures = [0, 1]\nchasers = [5]\ncue_lists = [6]\n").is_err());
        assert!(Animation::parse("version = 2\n").is_err());
    }
}
