//! OpenRGB output: the receiver's writer for QLC+ frames, and the header-probe diagnostic.
//!
//! The palette-driven `render-*` commands that used to live here (the pre-QLC direct-SDK
//! renderer) were removed on 2026-09-25: QLC+ is the sole authoring surface, and nothing
//! called them. Recover them from Git history if ever needed.

use crate::config::{self, Calibration, Gain, Layout, Zone};
use crate::e131::QlcFrame;
use openrgb2::{Color, Controller, OpenRgbClient};

const SDK_ADDRESS: &str = "127.0.0.1:6742";

struct Resolved<'a> {
    ram: Vec<&'a Controller>,
    rog_eye: (&'a Controller, usize, usize),
    strip: (&'a Controller, usize, usize),
}

fn configured_zone<'a>(layout: &'a Layout, name: &str) -> Result<&'a Zone, String> {
    let zone = layout.zones.get(name).ok_or_else(|| format!("missing zone {name}"))?;
    if !zone.available { return Err(format!("zone {name} is unavailable")); }
    Ok(zone)
}

fn matches_ram(controller: &Controller, zone: &Zone) -> bool {
    zone.vendor.as_deref().is_none_or(|vendor| controller.vendor() == vendor)
        && zone.location_prefix.as_deref().is_none_or(|prefix| controller.location().starts_with(prefix))
}

fn resolve<'a>(layout: &Layout, controllers: &'a [Controller]) -> Result<Resolved<'a>, String> {
    let ram_zone = configured_zone(layout, "ram")?;
    if ram_zone.kind != "controllers" { return Err("ram must be a controllers zone".to_owned()); }
    let mut ram: Vec<&Controller> = controllers.iter().filter(|controller| matches_ram(controller, ram_zone)).collect();
    if ram.len() != 4 { return Err(format!("expected four RAM controllers, found {}", ram.len())); }
    if let Some(order) = &ram_zone.controller_ids {
        if order.len() != 4 { return Err("ram.controller_ids must list four SDK IDs".to_owned()); }
        let mut ordered = Vec::with_capacity(4);
        for id in order {
            ordered.push(ram.iter().copied().find(|controller| controller.id() == *id).ok_or_else(|| format!("configured RAM SDK ID {id} is missing"))?);
        }
        ram = ordered;
    } else {
        ram.sort_by_key(|controller| controller.id());
    }
    if ram.iter().any(|controller| controller.num_leds() != 8) { return Err("every RAM controller must expose eight LEDs".to_owned()); }

    let eye_zone = configured_zone(layout, "rog_eye")?;
    if eye_zone.kind != "zone" { return Err("rog_eye must be a zone".to_owned()); }
    let board = controllers.iter().find(|controller| {
        eye_zone.controller_vendor.as_deref().is_none_or(|vendor| controller.vendor() == vendor)
            && eye_zone.controller_serial.as_deref().is_none_or(|serial| controller.serial() == serial)
    }).ok_or_else(|| "configured ASUS ROG-eye controller is missing".to_owned())?;
    let index = eye_zone.zone_index.ok_or_else(|| "rog_eye.zone_index is required".to_owned())?;
    let led_count = board.get_zone(index).map_err(|error| error.to_string())?.num_leds();
    if led_count < 3 { return Err(format!("ROG eye zone has {led_count} LEDs; expected at least 3")); }

    let strip_zone = configured_zone(layout, "strip")?;
    if strip_zone.kind != "zone" { return Err("strip must be a zone".to_owned()); }
    let strip_index = strip_zone.zone_index.ok_or_else(|| "strip.zone_index is required".to_owned())?;
    let strip_led_count = board.get_zone(strip_index).map_err(|error| error.to_string())?.num_leds();
    let configured_strip_led_count = strip_zone.led_count.ok_or_else(|| "strip.led_count is required".to_owned())?;
    if strip_led_count != configured_strip_led_count {
        return Err(format!("strip has {strip_led_count} LEDs; expected {configured_strip_led_count}"));
    }
    Ok(Resolved { ram, rog_eye: (board, index, led_count), strip: (board, strip_index, strip_led_count) })
}

fn dmx_colors(slots: &[u8], gain: Gain) -> Result<Vec<Color>, String> {
    if slots.len() % 3 != 0 { return Err(format!("DMX RGB payload has {} slots, not a multiple of three", slots.len())); }
    Ok(slots
        .chunks_exact(3)
        .map(|rgb| {
            let [red, green, blue] = gain.apply([rgb[0], rgb[1], rgb[2]]);
            Color::new(red, green, blue)
        })
        .collect())
}

pub struct QlcOutput {
    layout: Layout,
    controllers: Vec<Controller>,
}

impl QlcOutput {
    pub async fn connect() -> Result<Self, String> {
        let layout = config::load_layout(&crate::paths::config_dir().join("scene-layout.toml"))?;
        let client = OpenRgbClient::connect_to(SDK_ADDRESS, 6).await.map_err(|error| error.to_string())?;
        let controllers = client.get_all_controllers().await.map_err(|error| error.to_string())?.into_iter().collect::<Vec<_>>();
        let resolved = resolve(&layout, &controllers)?;
        if layout.qlc_e131.ram_controller_ids.len() != 4 { return Err("qlc_e131.ram_controller_ids must list four SDK IDs".to_owned()); }
        if layout.qlc_e131.board_universe != 5 { return Err("qlc_e131.board_universe must be 5".to_owned()); }
        for controller_id in &layout.qlc_e131.ram_controller_ids {
            if !resolved.ram.iter().any(|controller| controller.id() == *controller_id) {
                return Err(format!("QLC E1.31 maps missing or non-RAM SDK controller {controller_id}"));
            }
        }
        direct_mode(&resolved).await?;
        Ok(Self { layout, controllers })
    }

    pub async fn apply(&self, frame: &QlcFrame, calibration: &Calibration) -> Result<(), String> {
        let resolved = resolve(&self.layout, &self.controllers)?;
        for (offset, controller_id) in self.layout.qlc_e131.ram_controller_ids.iter().copied().enumerate() {
            let controller = resolved.ram.iter().copied().find(|controller| controller.id() == controller_id)
                .ok_or_else(|| format!("configured QLC RAM SDK ID {controller_id} is missing"))?;
            let colors = dmx_colors(frame.universe((offset + 1) as u16)?, calibration.gain("ram"))?;
            if colors.len() != controller.num_leds() {
                return Err(format!("QLC universe {} has {} RGB pixels; RAM SDK ID {controller_id} has {} LEDs", offset + 1, colors.len(), controller.num_leds()));
            }
            controller.set_leds(colors).await.map_err(|error| error.to_string())?;
        }

        let board_slots = frame.universe(self.layout.qlc_e131.board_universe)?;
        let eye_colors = dmx_colors(&board_slots[..15], calibration.gain("rog_eye"))?;
        let (board, eye_zone, eye_led_count) = resolved.rog_eye;
        if eye_led_count != eye_colors.len() {
            return Err(format!("ROG eye has {eye_led_count} logical LEDs; QLC board prefix has {}", eye_colors.len()));
        }
        board.set_zone_leds(eye_zone, eye_colors).await.map_err(|error| error.to_string())?;

        let strip_colors = dmx_colors(&board_slots[15..], calibration.gain("strip"))?;
        let (strip_board, strip_zone, strip_led_count) = resolved.strip;
        if strip_colors.len() != strip_led_count {
            return Err(format!("strip has {strip_led_count} LEDs; QLC board payload has {}", strip_colors.len()));
        }
        strip_board.set_zone_leds(strip_zone, strip_colors).await.map_err(|error| error.to_string())
    }
}

impl crate::supervisor::Output for QlcOutput {
    type Frame = QlcFrame;
    type Context = Calibration;

    fn apply(&self, frame: &QlcFrame, calibration: &Calibration) -> impl std::future::Future<Output = Result<(), String>> {
        QlcOutput::apply(self, frame, calibration)
    }
}

async fn direct_mode(resolved: &Resolved<'_>) -> Result<(), String> {
    for controller in &resolved.ram { controller.set_controllable_mode().await.map_err(|error| error.to_string())?; }
    resolved.rog_eye.0.set_controllable_mode().await.map_err(|error| error.to_string())?;
    Ok(())
}

/// How many LEDs of each RAM stick light to display the number `n`, left to right.
fn stick_levels(n: usize) -> [usize; 4] {
    std::array::from_fn(|position| n.saturating_sub(position * 8).min(8))
}

/// How many of the eye's three visible LEDs light to display `n`: one per full 32.
fn eye_blocks(n: usize) -> usize {
    (n / 32).min(3)
}

/// A header with exactly one LED lit; `number` is 1-based.
fn single_led(count: usize, number: usize, lit: Color, dark: Color) -> Vec<Color> {
    (0..count).map(|index| if index + 1 == number { lit } else { dark }).collect()
}

/// Diagnostic for finding where one device on the ARGB header ends and the next begins.
///
/// Lights LED `number` of the header (raw white, no calibration) for `dwell`, for each
/// number in turn, while RAM and the eye display that number so a person can read it:
/// RAM fills 8 LEDs per stick left to right (green), and each full 32 lights one of the
/// eye's LEDs (blue). Needs OpenRGB reachable from the host, so the lighting stack must
/// be stopped and the standalone SDK server running (see probe-header.sh).
pub async fn probe_header(numbers: std::ops::RangeInclusive<usize>, dwell: std::time::Duration) -> Result<(), String> {
    let (layout, controllers) = loop_until_discovered().await?;
    let resolved = resolve(&layout, &controllers)?;
    direct_mode(&resolved).await?;
    let (board, header_zone, count) = resolved.strip;
    let (_, eye_zone, eye_count) = resolved.rog_eye;
    if *numbers.start() == 0 || *numbers.end() > count {
        return Err(format!("numbers must be 1 through {count} (the header is configured for {count} LEDs)"));
    }
    let (off, white, green, blue) = (Color::new(0, 0, 0), Color::new(255, 255, 255), Color::new(0, 255, 0), Color::new(0, 0, 255));
    for number in numbers {
        board.set_zone_leds(header_zone, single_led(count, number, white, off)).await.map_err(|error| error.to_string())?;
        for (controller, level) in resolved.ram.iter().zip(stick_levels(number)) {
            let colors = (0..controller.num_leds()).map(|index| if index + level >= controller.num_leds() { green } else { off }).collect::<Vec<_>>();
            controller.set_leds(colors).await.map_err(|error| error.to_string())?;
        }
        let blocks = eye_blocks(number);
        let eye = (0..eye_count).map(|index| if index < 3 && index < blocks { blue } else { off }).collect::<Vec<_>>();
        board.set_zone_leds(eye_zone, eye).await.map_err(|error| error.to_string())?;
        println!("LED {number}");
        tokio::time::sleep(dwell).await;
    }
    board.set_zone_leds(header_zone, vec![off; count]).await.map_err(|error| error.to_string())?;
    board.set_zone_leds(eye_zone, vec![off; eye_count]).await.map_err(|error| error.to_string())?;
    for controller in &resolved.ram {
        controller.set_all_leds(off).await.map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// A freshly started OpenRGB takes several seconds to detect its devices; wait for the
/// Monolith controllers to appear (up to 40 s).
async fn loop_until_discovered() -> Result<(Layout, Vec<Controller>), String> {
    let mut last = String::from("no attempt made");
    for _ in 0..80 {
        let attempt = async {
            let layout = config::load_layout(&crate::paths::config_dir().join("scene-layout.toml"))?;
            let client = OpenRgbClient::connect_to(SDK_ADDRESS, 6).await.map_err(|error| error.to_string())?;
            let controllers: Vec<Controller> = client.get_all_controllers().await.map_err(|error| error.to_string())?.into_iter().collect();
            resolve(&layout, &controllers)?;
            Ok::<_, String>((layout, controllers))
        };
        match attempt.await {
            Ok(found) => return Ok(found),
            Err(error) => last = error,
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Err(format!("OpenRGB did not expose the Monolith controllers: {last}"))
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    #[test]
    fn ram_sticks_display_a_number_eight_leds_at_a_time() {
        assert_eq!(stick_levels(1), [1, 0, 0, 0]);
        assert_eq!(stick_levels(8), [8, 0, 0, 0]);
        assert_eq!(stick_levels(13), [8, 5, 0, 0]);
        assert_eq!(stick_levels(32), [8, 8, 8, 8]);
        assert_eq!(stick_levels(40), [8, 8, 8, 8], "RAM saturates at 32; the eye takes over");
    }

    #[test]
    fn the_eye_counts_full_blocks_of_thirty_two() {
        assert_eq!([31, 32, 63, 64, 70, 96, 200].map(eye_blocks), [0, 1, 1, 2, 2, 3, 3]);
    }

    #[test]
    fn exactly_one_header_led_is_lit_and_numbers_are_one_based() {
        let (lit, dark) = (Color::new(255, 255, 255), Color::new(0, 0, 0));
        let leds = single_led(5, 3, lit, dark);
        assert_eq!(leds.iter().map(|c| c.r).collect::<Vec<_>>(), vec![0, 0, 255, 0, 0]);
        assert_eq!(single_led(5, 1, lit, dark)[0].r, 255);
        assert!(single_led(5, 6, lit, dark).iter().all(|c| c.r == 0));
    }
}

#[cfg(test)]
mod calibration_reach_tests {
    use super::*;

    #[test]
    fn gain_is_applied_to_every_decoded_pixel() {
        let colors = dmx_colors(&[255, 255, 255, 10, 200, 30], Gain([1.0, 0.5, 1.0])).unwrap();
        assert_eq!(colors.len(), 2);
        let expected = [Color::new(255, 128, 255), Color::new(10, 100, 30)];
        for (got, want) in colors.iter().zip(expected) {
            assert_eq!((got.r, got.g, got.b), (want.r, want.g, want.b));
        }
        assert!(dmx_colors(&[1, 2], Gain::UNITY).is_err());
    }

}
