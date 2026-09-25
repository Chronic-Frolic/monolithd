//! Gamepad activity from evdev, for the suspend policy.
//!
//! Only devices udev marks `ID_INPUT_JOYSTICK=1` are opened; keyboards and mice never are
//! (KWin's idle signal covers those, and reading them directly would be keylogger-grade
//! access). The seat user can read joysticks through the ACL logind grants for `uaccess`
//! devices, so no permission change is needed. Devices are read non-blocking and never
//! grabbed, so games and Steam Input see every event as before.
//!
//! A button press counts, and so does a stick or trigger moving more than a tenth of its
//! range since the last counted position; analog sticks jitter slightly at rest, which
//! would otherwise keep the machine awake forever.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
/// `struct input_event` on 64-bit Linux: a 16-byte timeval, then type, code and value.
const EVENT_SIZE: usize = 24;
/// Fraction of an axis's range it must move to count.
const AXIS_DEADZONE: i32 = 10;

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

/// Does udev's database entry for this character device mark it as a joystick?
fn is_joystick(udev_data: &str) -> bool {
    udev_data.lines().any(|line| line.trim() == "E:ID_INPUT_JOYSTICK=1")
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
    axes: BTreeMap<u16, (i32, i32)>,
}

impl Judge {
    /// `range` is asked for only the first time an axis is seen.
    fn significant(&mut self, event: Event, range: impl FnOnce(u16) -> Option<(i32, i32)>) -> bool {
        match event.kind {
            EV_KEY => event.value == 1,
            EV_ABS => match self.axes.get_mut(&event.code) {
                Some((last, threshold)) => {
                    if (event.value - *last).abs() >= *threshold {
                        *last = event.value;
                        true
                    } else {
                        false
                    }
                }
                None => {
                    let (min, max) = range(event.code).unwrap_or((-32768, 32767));
                    let threshold = ((max - min) / AXIS_DEADZONE).max(1);
                    self.axes.insert(event.code, (event.value, threshold));
                    false
                }
            },
            _ => false,
        }
    }
}

struct Pad {
    file: File,
    name: String,
    judge: Judge,
}

/// All joysticks, rescanned for hotplug.
#[derive(Default)]
pub struct Gamepads {
    pads: BTreeMap<PathBuf, Pad>,
}

impl Gamepads {
    /// Open joysticks that appeared since the last scan.
    pub fn rescan(&mut self) {
        for entry in std::fs::read_dir("/sys/class/input").into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("event") {
                continue;
            }
            let device = PathBuf::from("/dev/input").join(&name);
            if self.pads.contains_key(&device) {
                continue;
            }
            let Ok(numbers) = std::fs::read_to_string(entry.path().join("dev")) else { continue };
            let udev = std::fs::read_to_string(Path::new("/run/udev/data").join(format!("c{}", numbers.trim()))).unwrap_or_default();
            if !is_joystick(&udev) {
                continue;
            }
            let Ok(file) = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(&device) else { continue };
            let label = std::fs::read_to_string(entry.path().join("device/name")).unwrap_or_else(|_| name.clone()).trim().to_owned();
            self.pads.insert(device, Pad { file, name: label, judge: Judge::default() });
        }
    }

    /// Read everything pending. Returns the name of a pad that was used, if any; a pad that
    /// was unplugged is dropped.
    pub fn poll(&mut self) -> Option<String> {
        let mut used = None;
        let mut gone = Vec::new();
        for (path, pad) in &mut self.pads {
            let mut buffer = [0u8; EVENT_SIZE * 64];
            loop {
                match pad.file.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        for event in parse_events(&buffer[..count]) {
                            let file = &pad.file;
                            if pad.judge.significant(event, |axis| axis_range(file, axis)) {
                                used = Some(pad.name.clone());
                            }
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
            self.pads.remove(&path);
        }
        used
    }

    pub fn names(&self) -> Vec<String> {
        self.pads.values().map(|pad| pad.name.clone()).collect()
    }
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
    fn recognises_joysticks_from_udev_data() {
        assert!(is_joystick("I:123\nE:ID_INPUT=1\nE:ID_INPUT_JOYSTICK=1\nG:uaccess\n"));
        assert!(!is_joystick("I:123\nE:ID_INPUT=1\nE:ID_INPUT_KEYBOARD=1\n"));
    }

    #[test]
    fn the_ioctl_number_matches_linux_headers() {
        // EVIOCGABS(ABS_X) from <linux/input.h> is 0x80184540.
        assert_eq!(eviocgabs(0), 0x8018_4540);
    }

    #[test]
    fn button_presses_count_but_releases_and_repeats_do_not() {
        let mut judge = Judge::default();
        assert!(judge.significant(Event { kind: EV_KEY, code: 304, value: 1 }, |_| None));
        assert!(!judge.significant(Event { kind: EV_KEY, code: 304, value: 0 }, |_| None));
        assert!(!judge.significant(Event { kind: EV_KEY, code: 304, value: 2 }, |_| None));
    }

    #[test]
    fn stick_jitter_is_ignored_and_real_movement_counts() {
        let mut judge = Judge::default();
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
        let mut judge = Judge::default();
        let trigger = |value| Event { kind: EV_ABS, code: 2, value };
        assert!(!judge.significant(trigger(0), |_| Some((0, 255))));
        assert!(!judge.significant(trigger(20), |_| None));
        assert!(judge.significant(trigger(40), |_| None), "25 is a tenth of 0..255");
    }
}
