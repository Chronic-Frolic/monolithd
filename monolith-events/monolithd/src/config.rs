use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

#[derive(Debug, Deserialize)]
pub struct PaletteFile {
    pub colors: Palette,
    pub fault_presentation: FaultPresentation,
    pub fallbacks: Fallbacks,
}

#[derive(Debug, Deserialize)]
pub struct Palette {
    pub off: Rgb,
    pub primary: Rgb,
    pub secondary: Rgb,
    pub warning: Rgb,
    pub fault: Rgb,
    pub controller_failure: Rgb,
}

#[derive(Debug, Deserialize)]
pub struct FaultPresentation {
    pub kind: String,
    pub interval_ms: u64,
    pub profile: String,
}

#[derive(Debug, Deserialize)]
pub struct Fallbacks {
    pub quiet: String,
    pub fault: String,
    pub controller_failure: String,
}

#[derive(Debug, Deserialize)]
pub struct Layout {
    pub zones: BTreeMap<String, Zone>,
    pub status_routes: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub idle_scene: IdleScene,
    pub qlc_e131: QlcE131,
}

#[derive(Debug, Deserialize)]
pub struct Zone {
    pub kind: String,
    pub available: bool,
    pub vendor: Option<String>,
    pub location_prefix: Option<String>,
    pub controller_vendor: Option<String>,
    pub controller_serial: Option<String>,
    pub zone_index: Option<usize>,
    pub controller_ids: Option<Vec<usize>>,
    pub led_count: Option<usize>,
    pub progress_direction: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct QlcE131 {
    pub listener: String,
    pub web_listener: String,
    pub startup_function: u32,
    pub ram_controller_ids: Vec<usize>,
    pub board_universe: u16,
}

#[derive(Debug, Deserialize)]
pub struct IdleScene {
    #[serde(default = "direct_scene")]
    pub kind: String,
    pub profile: Option<String>,
}

fn direct_scene() -> String { "direct".to_owned() }

impl Default for IdleScene {
    fn default() -> Self { Self { kind: direct_scene(), profile: None } }
}

pub fn load_layout(path: &Path) -> Result<Layout, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}

pub fn load_palette(path: &Path) -> Result<PaletteFile, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}

/// Per-channel gain for one zone: `[R, G, B]`, each 0.0 through 1.0.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct Gain(pub [f32; 3]);

impl Gain {
    pub const UNITY: Gain = Gain([1.0, 1.0, 1.0]);

    /// The gain rounded to six places, for reporting (an f32 prints as 0.1899999976).
    pub fn rounded(self) -> [f64; 3] {
        self.0.map(|value| (f64::from(value) * 1e6).round() / 1e6)
    }

    /// Scale a color in DMX space, rounding to the nearest level.
    pub fn apply(self, rgb: [u8; 3]) -> [u8; 3] {
        std::array::from_fn(|channel| (f32::from(rgb[channel]) * self.0[channel]).round().clamp(0.0, 255.0) as u8)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZoneCalibration {
    pub gain: Gain,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Calibration {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub zones: BTreeMap<String, ZoneCalibration>,
}

impl Calibration {
    /// A zone that is not listed runs at unity gain.
    pub fn gain(&self, zone: &str) -> Gain {
        self.zones.get(zone).map_or(Gain::UNITY, |entry| entry.gain)
    }

    pub fn describe(&self) -> String {
        let changed: Vec<String> = self
            .zones
            .iter()
            .filter(|(_, entry)| entry.gain != Gain::UNITY)
            .map(|(zone, entry)| format!("{zone} gain [{}, {}, {}]", entry.gain.0[0], entry.gain.0[1], entry.gain.0[2]))
            .collect();
        if changed.is_empty() { "all zones at unity gain".to_owned() } else { changed.join("; ") }
    }
}

/// Parse and validate calibration text against the zone names the layout defines.
pub fn parse_calibration(text: &str, known_zones: &BTreeSet<String>) -> Result<Calibration, String> {
    let calibration: Calibration = toml::from_str(text).map_err(|error| format!("parse: {error}"))?;
    if calibration.version != 1 {
        return Err(format!("version {} is unsupported (expected 1)", calibration.version));
    }
    for (zone, entry) in &calibration.zones {
        if !known_zones.contains(zone) {
            return Err(format!("unknown zone {zone}"));
        }
        for (channel, value) in entry.gain.0.iter().enumerate() {
            if !value.is_finite() || !(0.0..=1.0).contains(value) {
                return Err(format!("zone {zone}: gain {} is {value}, expected 0.0 through 1.0", ["R", "G", "B"][channel]));
            }
        }
    }
    Ok(calibration)
}

/// Whether the calibration file is in force, and why not when it is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationState {
    /// The watcher has not read the file yet.
    Unread,
    /// The file is valid and its gains are in force.
    Loaded,
    /// There is no file, so every zone runs at unity gain.
    Missing,
    /// The last read failed or failed validation; the previous gains stay in force.
    Invalid,
}

impl CalibrationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unread => "unread",
            Self::Loaded => "loaded",
            Self::Missing => "missing",
            Self::Invalid => "invalid",
        }
    }
}

/// What the receiver applies right now, published for the gateway's `status`.
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationReport {
    pub file: PathBuf,
    pub state: CalibrationState,
    /// Why the last read was rejected (only while `state` is `Invalid`).
    pub error: Option<String>,
    /// The gains in force.
    pub calibration: Calibration,
}

/// Read and validate the calibration file offline. A missing file is not an
/// error: it means every zone runs at unity gain.
pub fn check_calibration_file(path: &Path, layout: &Layout) -> Result<Option<Calibration>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let zones: BTreeSet<String> = layout.zones.keys().cloned().collect();
    parse_calibration(&text, &zones).map(Some).map_err(|error| format!("{}: {error}", path.display()))
}

/// Watches the calibration file and hands the receiver the gains currently in force.
///
/// The file is small, so it is re-read on every poll and acted on only when its
/// contents change. A bad edit never disturbs the gains already in force. Every
/// change of state is also published on a watch channel, so the gateway can report
/// it without touching the receiver's loop, which stays the single writer.
pub struct CalibrationWatcher {
    path: PathBuf,
    zones: BTreeSet<String>,
    seen: Option<String>,
    polled: bool,
    current: Calibration,
    state: CalibrationState,
    error: Option<String>,
    report: watch::Sender<CalibrationReport>,
}

impl CalibrationWatcher {
    pub fn new(path: PathBuf, layout: &Layout) -> Self {
        let initial = CalibrationReport { file: path.clone(), state: CalibrationState::Unread, error: None, calibration: Calibration::default() };
        let (report, _) = watch::channel(initial);
        Self {
            path,
            zones: layout.zones.keys().cloned().collect(),
            seen: None,
            polled: false,
            current: Calibration::default(),
            state: CalibrationState::Unread,
            error: None,
            report,
        }
    }

    pub fn current(&self) -> &Calibration {
        &self.current
    }

    /// A live view of the report, for the gateway.
    pub fn subscribe(&self) -> watch::Receiver<CalibrationReport> {
        self.report.subscribe()
    }

    fn publish(&self) {
        self.report.send_replace(CalibrationReport {
            file: self.path.clone(),
            state: self.state,
            error: self.error.clone(),
            calibration: self.current.clone(),
        });
    }

    /// Record a rejected file. Returns the log line only the first time a reason appears.
    fn reject(&mut self, reason: String, log: String) -> Option<String> {
        let fresh = self.state != CalibrationState::Invalid || self.error.as_deref() != Some(reason.as_str());
        self.state = CalibrationState::Invalid;
        self.error = Some(reason);
        if fresh {
            self.publish();
        }
        fresh.then_some(log)
    }

    /// Re-read the file. Returns a log line when something worth reporting changed.
    pub fn poll(&mut self) -> Option<String> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                self.polled = false; // process the file again once it is readable
                let log = format!("cannot read {}: {error}; keeping previous gains", self.path.display());
                return self.reject(format!("cannot read: {error}"), log);
            }
        };
        if self.polled && text == self.seen {
            return None;
        }
        self.polled = true;
        self.seen = text.clone();
        match text {
            None => {
                self.current = Calibration::default();
                self.state = CalibrationState::Missing;
                self.error = None;
                self.publish();
                Some(format!("{} not found; all zones at unity gain", self.path.display()))
            }
            Some(text) => match parse_calibration(&text, &self.zones) {
                Ok(calibration) => {
                    let message = format!("loaded: {}", calibration.describe());
                    self.current = calibration;
                    self.state = CalibrationState::Loaded;
                    self.error = None;
                    self.publish();
                    Some(message)
                }
                Err(error) => {
                    let log = format!("ignoring invalid {} ({error}); keeping previous gains", self.path.display());
                    self.reject(error, log)
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf() }

    #[test]
    fn parses_versioned_configuration() {
        let root = root();
        let layout = load_layout(&root.join("scene-layout.toml")).unwrap();
        assert!(layout.zones["ram"].available);
        assert!(layout.zones["strip"].available);
        assert_eq!(layout.zones["strip"].led_count, Some(70));
        assert_eq!(layout.zones["strip"].progress_direction.as_deref(), Some("sdk_end_to_start"));
        assert_eq!(layout.idle_scene.kind, "direct");
        assert_eq!(layout.qlc_e131.listener, "127.0.0.1:5568");
        assert_eq!(layout.qlc_e131.web_listener, "127.0.0.1:9999");
        assert_eq!(layout.qlc_e131.startup_function, 106);
        assert_eq!(layout.qlc_e131.ram_controller_ids, vec![0, 1, 2, 3]);
        assert_eq!(layout.qlc_e131.board_universe, 5);
        let palette = load_palette(&root.join("rgb-palette.toml")).unwrap();
        assert_eq!(palette.colors.primary, Rgb(255, 255, 255));
        assert_eq!(palette.fault_presentation.interval_ms, 500);
    }
}

#[cfg(test)]
mod calibration_tests {
    use super::*;

    fn zones() -> BTreeSet<String> {
        ["ram", "rog_eye", "strip"].iter().map(|zone| (*zone).to_owned()).collect()
    }

    #[test]
    fn unity_gain_leaves_every_level_untouched() {
        for level in 0..=255u8 {
            assert_eq!(Gain::UNITY.apply([level, level, level]), [level, level, level]);
        }
    }

    #[test]
    fn gain_scales_and_rounds_each_channel_independently() {
        assert_eq!(Gain([1.0, 0.85, 1.0]).apply([255, 255, 255]), [255, 217, 255]); // 216.75 rounds up
        assert_eq!(Gain([0.5, 0.5, 0.5]).apply([255, 101, 1]), [128, 51, 1]);
        assert_eq!(Gain([0.0, 1.0, 1.0]).apply([200, 200, 200]), [0, 200, 200]);
        assert_eq!(Gain([1.0, 0.0, 1.0]).apply([0x32, 0, 0xff]), [0x32, 0, 0xff], "green gain cannot touch a pure violet");
    }

    #[test]
    fn parses_a_valid_file_and_defaults_unlisted_zones_to_unity() {
        let calibration = parse_calibration("version = 1\n[zones.strip]\ngain = [1.0, 0.8, 0.9]\n", &zones()).unwrap();
        assert_eq!(calibration.gain("strip"), Gain([1.0, 0.8, 0.9]));
        assert_eq!(calibration.gain("ram"), Gain::UNITY);
        assert_eq!(calibration.gain("not_a_zone"), Gain::UNITY);
    }

    #[test]
    fn rejects_bad_calibration_files() {
        for (text, needle) in [
            ("version = 1\n[zones.strip]\ngain = [1.0, 1.2, 1.0]\n", "expected 0.0 through 1.0"),
            ("version = 1\n[zones.strip]\ngain = [1.0, -0.1, 1.0]\n", "expected 0.0 through 1.0"),
            ("version = 1\n[zones.strip]\ngain = [nan, 1.0, 1.0]\n", "expected 0.0 through 1.0"),
            ("version = 1\n[zones.roof]\ngain = [1.0, 1.0, 1.0]\n", "unknown zone roof"),
            ("version = 2\n", "unsupported"),
            ("[zones.strip]\ngain = [1.0, 1.0, 1.0]\n", "unsupported"),
            ("version = 1\n[zones.strip]\ngain = [1.0, 1.0]\n", "parse"),
            ("version = 1\n[zones.strip]\ngain = [1.0, 1.0, 1.0]\nextra = 1\n", "parse"),
            ("version = 1\nsurprise = true\n", "parse"),
        ] {
            let error = parse_calibration(text, &zones()).unwrap_err();
            assert!(error.contains(needle), "{text:?} -> {error}");
        }
    }

    #[test]
    fn the_shipped_calibration_file_is_valid_and_lists_every_zone() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("led-calibration.toml");
        let calibration = parse_calibration(&std::fs::read_to_string(path).unwrap(), &zones()).unwrap();
        for zone in ["ram", "rog_eye", "strip"] {
            assert!(calibration.zones.contains_key(zone), "{zone} should be listed");
        }
    }

    fn test_layout() -> Layout {
        toml::from_str(
            "[zones.ram]\nkind=\"controllers\"\navailable=true\n[zones.rog_eye]\nkind=\"zone\"\navailable=true\n[zones.strip]\nkind=\"zone\"\navailable=true\n[status_routes]\n[qlc_e131]\nlistener=\"127.0.0.1:5568\"\nweb_listener=\"127.0.0.1:9999\"\nstartup_function=106\nram_controller_ids=[0,1,2,3]\nboard_universe=5\n",
        )
        .unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("monolithd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn the_watcher_follows_the_file_publishes_its_state_and_survives_bad_edits() {
        let directory = scratch("calibration-watch");
        let path = directory.join("led-calibration.toml");
        let mut watcher = CalibrationWatcher::new(path.clone(), &test_layout());
        let report = watcher.subscribe();
        assert_eq!(report.borrow().state, CalibrationState::Unread);

        assert!(watcher.poll().unwrap().contains("unity gain"), "a missing file is reported once");
        assert_eq!(report.borrow().state, CalibrationState::Missing);
        assert_eq!(watcher.poll(), None);
        assert_eq!(watcher.current().gain("strip"), Gain::UNITY);

        std::fs::write(&path, "version = 1\n[zones.strip]\ngain = [1.0, 0.8, 1.0]\n").unwrap();
        assert!(watcher.poll().unwrap().contains("strip gain [1, 0.8, 1]"));
        assert_eq!(report.borrow().state, CalibrationState::Loaded);
        assert_eq!(report.borrow().calibration.gain("strip"), Gain([1.0, 0.8, 1.0]));
        assert_eq!(watcher.poll(), None, "an unchanged file is not reported again");

        std::fs::write(&path, "version = 1\n[zones.strip]\ngain = [1.0, 7.0, 1.0]\n").unwrap();
        assert!(watcher.poll().unwrap().contains("keeping previous gains"));
        assert_eq!(watcher.current().gain("strip"), Gain([1.0, 0.8, 1.0]), "a bad edit must not disturb the gains in force");
        {
            let seen = report.borrow();
            assert_eq!(seen.state, CalibrationState::Invalid);
            assert!(seen.error.as_deref().unwrap().contains("expected 0.0 through 1.0"), "{:?}", seen.error);
            assert_eq!(seen.calibration.gain("strip"), Gain([1.0, 0.8, 1.0]), "the report shows the gains actually in force");
        }
        assert_eq!(watcher.poll(), None);

        std::fs::write(&path, "version = 1\n[zones.strip]\ngain = [1.0, 0.7, 1.0]\n").unwrap();
        assert!(watcher.poll().is_some());
        assert_eq!((report.borrow().state, report.borrow().error.clone()), (CalibrationState::Loaded, None));
        assert_eq!(watcher.current().gain("strip"), Gain([1.0, 0.7, 1.0]));

        std::fs::remove_file(&path).unwrap();
        assert!(watcher.poll().unwrap().contains("unity gain"));
        assert_eq!(report.borrow().state, CalibrationState::Missing);
        assert_eq!(watcher.current().gain("strip"), Gain::UNITY);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_unreadable_file_is_flagged_and_recovers_when_readable_again() {
        let directory = scratch("calibration-unreadable");
        let path = directory.join("led-calibration.toml");
        let mut watcher = CalibrationWatcher::new(path.clone(), &test_layout());
        let report = watcher.subscribe();
        std::fs::write(&path, "version = 1\n[zones.strip]\ngain = [1.0, 0.8, 1.0]\n").unwrap();
        assert!(watcher.poll().is_some());

        // A directory where the file should be: reading it fails, but not with NotFound.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(watcher.poll().unwrap().contains("cannot read"));
        assert_eq!(report.borrow().state, CalibrationState::Invalid);
        assert_eq!(watcher.current().gain("strip"), Gain([1.0, 0.8, 1.0]), "gains in force are kept");
        assert_eq!(watcher.poll(), None, "the same failure is not logged every poll");

        // The same good contents come back: the state must recover even though the text is unchanged.
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, "version = 1\n[zones.strip]\ngain = [1.0, 0.8, 1.0]\n").unwrap();
        assert!(watcher.poll().unwrap().contains("loaded"));
        assert_eq!((report.borrow().state, report.borrow().error.clone()), (CalibrationState::Loaded, None));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_offline_check_distinguishes_missing_valid_and_invalid_files() {
        let directory = scratch("calibration-check");
        let path = directory.join("led-calibration.toml");
        let layout = test_layout();
        assert_eq!(check_calibration_file(&path, &layout), Ok(None));
        std::fs::write(&path, "version = 1\n[zones.strip]\ngain = [1.0, 0.8, 1.0]\n").unwrap();
        assert_eq!(check_calibration_file(&path, &layout).unwrap().unwrap().gain("strip"), Gain([1.0, 0.8, 1.0]));
        std::fs::write(&path, "version = 1\n[zones.roof]\ngain = [1.0, 1.0, 1.0]\n").unwrap();
        assert!(check_calibration_file(&path, &layout).unwrap_err().contains("unknown zone roof"));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn rounds_gains_for_reporting() {
        assert_eq!(Gain([0.19, 0.18, 1.0]).rounded(), [0.19, 0.18, 1.0]);
    }
}
