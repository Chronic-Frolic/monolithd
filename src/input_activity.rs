//! `monolithd input-activity`: when anyone last touched this machine, for the sleep policy.
//!
//! KWin and gamescope know about input because they read the kernel's input devices. This
//! reads the same devices, so one signal covers Desktop Mode, Gaming Mode, Moonlight's
//! virtual keyboard and mouse (Sunshine's uinput devices) and gamepads, with no compositor
//! protocol involved. gamescope has no idle protocol at all (measured 2026-09-26), which is
//! why this replaced the Wayland idle client and the gamepad reader (owner decision
//! 2026-09-26).
//!
//! It runs as its own system service (`systemd/root/monolith-input.service`): a throwaway
//! `DynamicUser` whose only extra right is the `input` group, with no network and a
//! read-only filesystem. Keystrokes never leave it: every event is reduced to "input
//! happened now", and the only output is `/run/monolith-input/activity`, world-readable
//! JSON holding the time of the last input, a heartbeat and a device count. monolithd
//! itself never opens an input device.
//!
//! What counts: a key or button press; relative motion (mice, wheels); a joystick axis
//! moving more than a tenth of its range since its last counted position (analog sticks
//! jitter at rest, which would otherwise keep the machine awake forever); any change of
//! another absolute axis (touchpads, tablets, Moonlight's absolute mouse). Devices are read
//! non-blocking and never grabbed, so every other reader sees every event as before.

use serde_json::json;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Where the service writes, and so where the sleep policy reads.
pub const ACTIVITY_FILE: &str = "/run/monolith-input/activity";

const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
/// `struct input_event` on 64-bit Linux: a 16-byte timeval, then type, code and value.
const EVENT_SIZE: usize = 24;
/// Fraction of a joystick axis's range it must move to count.
const AXIS_DEADZONE: i32 = 10;
const RESCAN: Duration = Duration::from_secs(5);
/// The file is rewritten at least this often, so a reader can tell the service is alive.
pub const HEARTBEAT: Duration = Duration::from_secs(15);
/// ...and at most once a second while input keeps arriving.
const WRITE_GAP: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Event {
    kind: u16,
    code: u16,
    value: i32,
}

fn parse_events(bytes: &[u8]) -> Vec<Event> {
    bytes
        .chunks_exact(EVENT_SIZE)
        .map(|raw| Event {
            kind: u16::from_ne_bytes([raw[16], raw[17]]),
            code: u16::from_ne_bytes([raw[18], raw[19]]),
            value: i32::from_ne_bytes([raw[20], raw[21], raw[22], raw[23]]),
        })
        .collect()
}

/// Whether udev's database entry marks a device as something a person uses: `Some(true)`
/// for a joystick (its axes need a deadzone), `Some(false)` for another input device,
/// `None` for anything else (switches, accelerometers, ...).
fn kind_of(udev_data: &str) -> Option<bool> {
    let has = |key: &str| udev_data.lines().any(|line| line.trim() == format!("E:{key}=1"));
    if has("ID_INPUT_ACCELEROMETER") {
        return None;
    }
    if has("ID_INPUT_JOYSTICK") {
        return Some(true);
    }
    ["ID_INPUT_KEYBOARD", "ID_INPUT_KEY", "ID_INPUT_MOUSE", "ID_INPUT_TOUCHPAD", "ID_INPUT_TABLET", "ID_INPUT_TOUCHSCREEN"].into_iter().any(has).then_some(false)
}

/// `EVIOCGABS(axis)`: `_IOR('E', 0x40 + axis, struct input_absinfo)`, six i32 values.
fn eviocgabs(axis: u16) -> u64 {
    (2 << 30) | (24 << 16) | (u64::from(b'E') << 8) | (0x40 + u64::from(axis))
}

/// The minimum and maximum of one absolute axis.
fn axis_range(file: &File, axis: u16) -> Option<(i32, i32)> {
    let mut info = [0i32; 6];
    // SAFETY: EVIOCGABS writes one `struct input_absinfo` (six i32) into `info`.
    let result = unsafe { libc::ioctl(file.as_raw_fd(), eviocgabs(axis) as _, info.as_mut_ptr()) };
    (result >= 0).then_some((info[1], info[2]))
}

/// Whether events are activity. Axes remember their last counted position and threshold.
#[derive(Default)]
struct Judge {
    joystick: bool,
    axes: BTreeMap<u16, (i32, i32)>,
}

impl Judge {
    /// `range` is asked for only the first time an axis is seen.
    fn significant(&mut self, event: Event, range: impl FnOnce(u16) -> Option<(i32, i32)>) -> bool {
        match event.kind {
            EV_KEY => event.value == 1,
            EV_REL => event.value != 0,
            EV_ABS => match self.axes.get_mut(&event.code) {
                Some((last, threshold)) => {
                    if (event.value - *last).abs() >= *threshold {
                        *last = event.value;
                        true
                    } else {
                        false
                    }
                }
                // The first sighting only calibrates.
                None => {
                    let threshold = if self.joystick {
                        let (min, max) = range(event.code).unwrap_or((-32768, 32767));
                        ((max - min) / AXIS_DEADZONE).max(1)
                    } else {
                        1
                    };
                    self.axes.insert(event.code, (event.value, threshold));
                    false
                }
            },
            _ => false,
        }
    }
}

struct Device {
    file: File,
    judge: Judge,
}

/// Every input device a person uses, rescanned for hotplug.
#[derive(Default)]
struct Devices {
    open: BTreeMap<PathBuf, Device>,
}

impl Devices {
    fn rescan(&mut self) {
        for entry in std::fs::read_dir("/sys/class/input").into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = PathBuf::from("/dev/input").join(&name);
            if !name.starts_with("event") || self.open.contains_key(&path) {
                continue;
            }
            let Ok(numbers) = std::fs::read_to_string(entry.path().join("dev")) else { continue };
            let udev = std::fs::read_to_string(Path::new("/run/udev/data").join(format!("c{}", numbers.trim()))).unwrap_or_default();
            let Some(joystick) = kind_of(&udev) else { continue };
            let Ok(file) = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(&path) else { continue };
            self.open.insert(path, Device { file, judge: Judge { joystick, axes: BTreeMap::new() } });
        }
    }

    /// Wait up to `timeout` for events, read everything pending, and report whether any of
    /// it was activity. A device that was unplugged is dropped.
    fn wait(&mut self, timeout: Duration) -> bool {
        let mut fds: Vec<libc::pollfd> = self.open.values().map(|device| libc::pollfd { fd: device.file.as_raw_fd(), events: libc::POLLIN, revents: 0 }).collect();
        // SAFETY: `fds` is a valid array of `fds.len()` pollfd structs for the whole call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout.as_millis() as libc::c_int) };
        if ready <= 0 {
            return false;
        }
        let mut active = false;
        let mut gone = Vec::new();
        for (path, device) in &mut self.open {
            let mut buffer = [0u8; EVENT_SIZE * 64];
            loop {
                match device.file.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        for event in parse_events(&buffer[..count]) {
                            let file = &device.file;
                            active |= device.judge.significant(event, |axis| axis_range(file, axis));
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        gone.push(path.clone());
                        break;
                    }
                }
            }
        }
        for path in gone {
            self.open.remove(&path);
        }
        active
    }
}

fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64()
}

/// Replace the activity file atomically, world-readable.
fn write(path: &Path, started: f64, last_input: Option<f64>, devices: usize) -> std::io::Result<()> {
    let body = json!({ "version": 1, "started_unix": started, "updated_unix": unix_now(), "last_input_unix": last_input, "devices": devices });
    let temporary = path.with_file_name(".activity.tmp");
    std::fs::write(&temporary, format!("{body}\n"))?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o644))?;
    std::fs::rename(&temporary, path)
}

fn serve(path: &Path) -> Result<(), String> {
    let started = unix_now();
    let mut devices = Devices::default();
    let mut last_input = None;
    let (mut rescanned, mut written): (Option<Instant>, Option<Instant>) = (None, None);
    let mut dirty = true;
    eprintln!("monolithd input-activity: writing {}", path.display());
    loop {
        if rescanned.is_none_or(|at| at.elapsed() >= RESCAN) {
            let before = devices.open.len();
            devices.rescan();
            if devices.open.len() != before {
                eprintln!("monolithd input-activity: watching {} input devices", devices.open.len());
                dirty = true;
            }
            rescanned = Some(Instant::now());
        }
        if devices.wait(Duration::from_secs(1)) {
            last_input = Some(unix_now());
            dirty = true;
        }
        let since = written.map(|at| at.elapsed());
        if since.is_none_or(|since| since >= HEARTBEAT || (dirty && since >= WRITE_GAP)) {
            if let Err(error) = write(path, started, last_input, devices.open.len()) {
                eprintln!("monolithd input-activity: write {}: {error}", path.display());
            }
            written = Some(Instant::now());
            dirty = false;
        }
    }
}

/// `monolithd input-activity [FILE]` (default `/run/monolith-input/activity`).
pub async fn run(arguments: Vec<String>) -> Result<(), String> {
    let path = PathBuf::from(match arguments.as_slice() {
        [] => ACTIVITY_FILE.to_owned(),
        [file] => file.clone(),
        _ => return Err("usage: monolithd input-activity [FILE]".to_owned()),
    });
    tokio::task::spawn_blocking(move || serve(&path)).await.map_err(|error| error.to_string())?
}

/// What the sleep policy learns from the activity file: seconds since the last input
/// (`None` when there has been none since the service started) and the device count.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub since_input: Option<f64>,
    pub devices: u64,
}

/// Read the activity file's text as of `now` (unix seconds). A file the service stopped
/// rewriting means the service is gone, and is an error rather than a stale "idle".
pub fn read_activity(text: &str, now: f64) -> Result<Reading, String> {
    let body: serde_json::Value = serde_json::from_str(text).map_err(|error| format!("unreadable activity file: {error}"))?;
    if body["version"] != 1 {
        return Err(format!("unsupported activity file version {}", body["version"]));
    }
    let updated = body["updated_unix"].as_f64().ok_or("the activity file has no updated_unix")?;
    let silence = now - updated;
    if silence > 4.0 * HEARTBEAT.as_secs_f64() {
        return Err(format!("the input service stopped updating {} s ago", silence.round()));
    }
    let since_input = body["last_input_unix"].as_f64().map(|last| (now - last).max(0.0));
    Ok(Reading { since_input, devices: body["devices"].as_u64().unwrap_or(0) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(kind: u16, code: u16, value: i32) -> Vec<u8> {
        let mut bytes = vec![0u8; 16];
        bytes.extend_from_slice(&kind.to_ne_bytes());
        bytes.extend_from_slice(&code.to_ne_bytes());
        bytes.extend_from_slice(&value.to_ne_bytes());
        bytes
    }

    #[test]
    fn parses_input_events() {
        let mut bytes = raw(EV_KEY, 304, 1);
        bytes.extend(raw(EV_ABS, 0, -1200));
        assert_eq!(parse_events(&bytes), vec![Event { kind: EV_KEY, code: 304, value: 1 }, Event { kind: EV_ABS, code: 0, value: -1200 }]);
    }

    #[test]
    fn watches_what_people_touch_and_nothing_else() {
        assert_eq!(kind_of("I:1\nE:ID_INPUT=1\nE:ID_INPUT_JOYSTICK=1\nG:uaccess\n"), Some(true));
        assert_eq!(kind_of("I:1\nE:ID_INPUT=1\nE:ID_INPUT_KEY=1\nE:ID_INPUT_KEYBOARD=1\n"), Some(false));
        assert_eq!(kind_of("I:1\nE:ID_INPUT=1\nE:ID_INPUT_MOUSE=1\n"), Some(false), "Moonlight's virtual mice included");
        assert_eq!(kind_of("I:1\nE:ID_INPUT=1\nE:ID_INPUT_SWITCH=1\n"), None, "lid and jack switches are not people");
        assert_eq!(kind_of("I:1\nE:ID_INPUT=1\nE:ID_INPUT_ACCELEROMETER=1\n"), None);
    }

    #[test]
    fn the_ioctl_number_matches_linux_headers() {
        // EVIOCGABS(ABS_X) from <linux/input.h> is 0x80184540.
        assert_eq!(eviocgabs(0), 0x8018_4540);
    }

    #[test]
    fn presses_and_motion_count_but_releases_and_repeats_do_not() {
        let mut judge = Judge::default();
        assert!(judge.significant(Event { kind: EV_KEY, code: 30, value: 1 }, |_| None));
        assert!(!judge.significant(Event { kind: EV_KEY, code: 30, value: 0 }, |_| None));
        assert!(!judge.significant(Event { kind: EV_KEY, code: 30, value: 2 }, |_| None));
        assert!(judge.significant(Event { kind: EV_REL, code: 0, value: -3 }, |_| None), "mouse motion");
        assert!(!judge.significant(Event { kind: EV_REL, code: 0, value: 0 }, |_| None));
        assert!(!judge.significant(Event { kind: 0, code: 0, value: 0 }, |_| None), "sync reports are not input");
    }

    #[test]
    fn stick_jitter_is_ignored_and_real_movement_counts() {
        let mut judge = Judge { joystick: true, ..Judge::default() };
        let stick = |value| Event { kind: EV_ABS, code: 0, value };
        assert!(!judge.significant(stick(120), |_| Some((-32768, 32767))), "first sighting only calibrates");
        for jitter in [-300, 400, 90, -1500] {
            assert!(!judge.significant(stick(jitter), |_| panic!("range asked once")));
        }
        assert!(judge.significant(stick(9000), |_| None), "a real push, over 10% of the range");
        assert!(!judge.significant(stick(9500), |_| None));
    }

    #[test]
    fn triggers_use_their_own_range() {
        let mut judge = Judge { joystick: true, ..Judge::default() };
        let trigger = |value| Event { kind: EV_ABS, code: 2, value };
        assert!(!judge.significant(trigger(0), |_| Some((0, 255))));
        assert!(!judge.significant(trigger(20), |_| None));
        assert!(judge.significant(trigger(40), |_| None), "25 is a tenth of 0..255");
    }

    #[test]
    fn pointer_axes_count_any_movement() {
        let mut judge = Judge::default();
        let x = |value| Event { kind: EV_ABS, code: 0, value };
        assert!(!judge.significant(x(500), |_| panic!("pointers need no range")));
        assert!(judge.significant(x(503), |_| None), "a small absolute-mouse move counts");
        assert!(!judge.significant(x(503), |_| None), "no movement, no activity");
    }

    #[test]
    fn writes_a_world_readable_file_the_reader_accepts() {
        let directory = std::env::temp_dir().join(format!("monolith-input-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("activity");
        write(&path, 100.0, Some(unix_now() - 30.0), 7).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
        let reading = read_activity(&std::fs::read_to_string(&path).unwrap(), unix_now()).unwrap();
        assert_eq!(reading.devices, 7);
        assert!((reading.since_input.unwrap() - 30.0).abs() < 2.0);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_silent_or_foreign_file_is_an_error() {
        let file = |updated: f64, last: &str| format!("{{\"version\":1,\"updated_unix\":{updated},\"last_input_unix\":{last},\"devices\":3}}");
        assert_eq!(read_activity(&file(1000.0, "null"), 1010.0).unwrap(), Reading { since_input: None, devices: 3 }, "no input since start");
        assert!(read_activity(&file(1000.0, "990"), 1100.0).unwrap_err().contains("stopped updating"));
        assert!(read_activity("{\"version\":2}", 0.0).is_err());
        assert!(read_activity("not json", 0.0).is_err());
    }
}
