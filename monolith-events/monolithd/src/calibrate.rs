//! `monolithd calibrate`: inspect and adjust `led-calibration.toml` from the host.
//!
//! Calibration is a property of the LED hardware, judged by eye, so it lives in
//! one file and is deliberately not reachable through the scene gateway. The
//! running lighting stack picks up an edited file within about a second.

use crate::config::{self, Gain};
use std::path::{Path, PathBuf};

const USAGE: &str = "usage: monolithd calibrate --show | monolithd calibrate ZONE R G B   (each gain 0.0 to 1.0)";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// Set one zone's `gain = [...]` line in the file text, leaving every comment
/// and every other line exactly as it was. A missing key or zone is added.
pub fn set_gain_text(text: &str, zone: &str, gain: [f32; 3]) -> String {
    let header = format!("[zones.{zone}]");
    let new_line = |indent: &str| format!("{indent}gain = [{:?}, {:?}, {:?}]", gain[0], gain[1], gain[2]);
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();

    let Some(start) = lines.iter().position(|line| line.trim() == header) else {
        while lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.pop();
        }
        lines.push(String::new());
        lines.push(header);
        lines.push(new_line(""));
        return lines.join("\n") + "\n";
    };
    let end = lines[start + 1..].iter().position(|line| line.trim_start().starts_with('[')).map_or(lines.len(), |offset| start + 1 + offset);
    match lines[start + 1..end].iter().position(|line| line.trim_start().starts_with("gain") && line.contains('=')) {
        Some(offset) => {
            let index = start + 1 + offset;
            let indent: String = lines[index].chars().take_while(|c| c.is_whitespace()).collect();
            lines[index] = new_line(&indent);
        }
        None => lines.insert(start + 1, new_line("")),
    }
    lines.join("\n") + "\n"
}

/// A one-line summary of what a zone's gain does to full white.
pub fn describe_zone(zone: &str, gain: Gain) -> String {
    let white = gain.apply([255, 255, 255]);
    format!("{zone:8} gain [{:?}, {:?}, {:?}]   full white becomes ({}, {}, {})", gain.0[0], gain.0[1], gain.0[2], white[0], white[1], white[2])
}

fn read_current(path: &Path, layout: &config::Layout) -> Result<Option<config::Calibration>, String> {
    config::check_calibration_file(path, layout)
}

fn show(path: &Path, layout: &config::Layout) -> Result<(), String> {
    let calibration = read_current(path, layout)?;
    match &calibration {
        None => println!("{} does not exist; every zone runs at unity gain", path.display()),
        Some(_) => println!("{}", path.display()),
    }
    let calibration = calibration.unwrap_or_default();
    for (zone, entry) in &layout.zones {
        if entry.available {
            println!("  {}", describe_zone(zone, calibration.gain(zone)));
        }
    }
    Ok(())
}

fn backup_directory() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/share/monolith-events/backups"))
}

fn backup(path: &Path, directory: &Path) -> Result<Option<PathBuf>, String> {
    if !path.exists() {
        return Ok(None);
    }
    std::fs::create_dir_all(directory).map_err(|error| format!("create {}: {error}", directory.display()))?;
    let seconds = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|error| error.to_string())?.as_secs();
    let target = directory.join(format!("led-calibration.toml.{seconds}"));
    std::fs::copy(path, &target).map_err(|error| format!("back up to {}: {error}", target.display()))?;
    Ok(Some(target))
}

fn set(path: &Path, layout: &config::Layout, zone: &str, values: [f32; 3], backups: &Path) -> Result<(), String> {
    let known: Vec<&str> = layout.zones.iter().filter(|(_, entry)| entry.available).map(|(name, _)| name.as_str()).collect();
    match layout.zones.get(zone) {
        None => return Err(format!("unknown zone {zone}; zones with hardware: {}", known.join(", "))),
        Some(entry) if !entry.available => return Err(format!("zone {zone} has no hardware mapped, so there is nothing to calibrate; zones with hardware: {}", known.join(", "))),
        Some(_) => {}
    }
    // Refuse to edit a file that is already broken: fix that first.
    read_current(path, layout)?;
    let known: std::collections::BTreeSet<String> = layout.zones.keys().cloned().collect();
    let text = std::fs::read_to_string(path).unwrap_or_else(|_| "version = 1\n".to_owned());
    let edited = set_gain_text(&text, zone, values);
    let calibration = config::parse_calibration(&edited, &known)?;
    if calibration.gain(zone) != Gain(values) {
        return Err("internal error: the edited file does not hold the requested gain".to_owned());
    }
    let saved = backup(path, backups)?;
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, &edited).map_err(|error| format!("write {}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|error| format!("replace {}: {error}", path.display()))?;
    if let Some(saved) = saved {
        println!("previous file saved to {}", saved.display());
    }
    println!("{}", describe_zone(zone, calibration.gain(zone)));
    println!("the running lighting stack applies it within about a second");
    Ok(())
}

pub fn run(arguments: Vec<String>) -> Result<(), String> {
    let root = root();
    let layout = config::load_layout(&root.join("scene-layout.toml"))?;
    let path = root.join("led-calibration.toml");
    let words: Vec<&str> = arguments.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["--show"] => show(&path, &layout),
        [zone, r, g, b] => {
            let value = |text: &str| text.parse::<f32>().map_err(|_| format!("{text:?} is not a number; {USAGE}"));
            set(&path, &layout, zone, [value(r)?, value(g)?, value(b)?], &backup_directory()?)
        }
        _ => Err(USAGE.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "\
# header comment
version = 1

# RAM is the reference.
[zones.ram]
gain = [1.0, 1.0, 1.0]

# Strip rationale that must survive edits.
[zones.strip]
gain = [0.19, 0.18, 1.0]
";

    fn zones() -> std::collections::BTreeSet<String> {
        ["ram", "rog_eye", "strip"].iter().map(|zone| (*zone).to_owned()).collect()
    }

    #[test]
    fn replaces_only_the_named_zones_gain_and_keeps_every_comment() {
        let edited = set_gain_text(FILE, "strip", [0.2, 0.17, 1.0]);
        assert_eq!(edited, FILE.replace("[0.19, 0.18, 1.0]", "[0.2, 0.17, 1.0]"));
        assert!(edited.contains("# Strip rationale that must survive edits."));
        let calibration = config::parse_calibration(&edited, &zones()).unwrap();
        assert_eq!(calibration.gain("strip"), Gain([0.2, 0.17, 1.0]));
        assert_eq!(calibration.gain("ram"), Gain::UNITY);
    }

    #[test]
    fn is_idempotent_when_the_value_is_unchanged() {
        assert_eq!(set_gain_text(FILE, "strip", [0.19, 0.18, 1.0]), FILE);
        assert_eq!(set_gain_text(FILE, "ram", [1.0, 1.0, 1.0]), FILE);
    }

    #[test]
    fn adds_a_missing_zone_at_the_end() {
        let edited = set_gain_text(FILE, "rog_eye", [1.0, 0.9, 0.8]);
        assert!(edited.starts_with(FILE.trim_end()), "existing text must be untouched");
        assert!(edited.ends_with("\n[zones.rog_eye]\ngain = [1.0, 0.9, 0.8]\n"), "{edited}");
        assert_eq!(config::parse_calibration(&edited, &zones()).unwrap().gain("rog_eye"), Gain([1.0, 0.9, 0.8]));
    }

    #[test]
    fn adds_a_missing_gain_key_inside_an_existing_zone() {
        let edited = set_gain_text("version = 1\n[zones.ram]\n", "ram", [0.5, 0.5, 0.5]);
        assert_eq!(edited, "version = 1\n[zones.ram]\ngain = [0.5, 0.5, 0.5]\n");
    }

    #[test]
    fn does_not_touch_another_zones_gain_line() {
        let edited = set_gain_text(FILE, "ram", [0.9, 0.9, 0.9]);
        assert!(edited.contains("[zones.strip]\ngain = [0.19, 0.18, 1.0]"));
    }

    #[test]
    fn whole_numbers_are_written_as_toml_floats() {
        assert!(set_gain_text(FILE, "ram", [1.0, 0.0, 1.0]).contains("gain = [1.0, 0.0, 1.0]"));
    }

    #[test]
    fn edited_out_of_range_gains_are_rejected_by_the_validator() {
        let edited = set_gain_text(FILE, "strip", [1.4, 0.2, 1.0]);
        assert!(config::parse_calibration(&edited, &zones()).unwrap_err().contains("expected 0.0 through 1.0"));
    }

    #[test]
    fn describes_what_a_gain_does_to_white() {
        assert_eq!(describe_zone("strip", Gain([0.19, 0.18, 1.0])), "strip    gain [0.19, 0.18, 1.0]   full white becomes (48, 46, 255)");
    }

    #[test]
    fn set_edits_a_real_file_atomically_and_refuses_bad_input() {
        let directory = std::env::temp_dir().join(format!("monolithd-calibrate-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("led-calibration.toml");
        std::fs::write(&path, FILE).unwrap();
        let root_layout = config::load_layout(&root().join("scene-layout.toml")).unwrap();
        let backups = directory.join("backups");

        set(&path, &root_layout, "strip", [0.21, 0.19, 1.0], &backups).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("gain = [0.21, 0.19, 1.0]") && text.contains("# Strip rationale that must survive edits."));
        assert!(!path.with_extension("toml.tmp").exists(), "no temporary file left behind");
        let saved: Vec<_> = std::fs::read_dir(&backups).unwrap().collect();
        assert_eq!(saved.len(), 1, "the previous file is backed up before it is replaced");

        assert!(set(&path, &root_layout, "roof", [1.0; 3], &backups).unwrap_err().contains("unknown zone roof"));
        assert!(set(&path, &root_layout, "gpu_bracket", [1.0; 3], &backups).unwrap_err().contains("no hardware mapped"));
        assert!(set(&path, &root_layout, "strip", [2.0, 0.2, 1.0], &backups).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "a refused edit leaves the file untouched");

        std::fs::write(&path, "version = 9\n").unwrap();
        assert!(set(&path, &root_layout, "strip", [0.2, 0.2, 1.0], &backups).unwrap_err().contains("unsupported"), "a broken file is not edited over");

        let _ = std::fs::remove_dir_all(&directory);
    }
}
