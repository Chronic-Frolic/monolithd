use crate::config::{self, Calibration, Gain, Layout, Palette, PaletteFile, Rgb, Zone};
use crate::e131::QlcFrame;
use openrgb2::{Color, Controller, OpenRgbClient};
use std::path::{Path, PathBuf};
use tokio::process::Command;

const OPENRGB: &str = "/home/chronic_frolic/AppImages/openrgb-1.0.appimage";
const SDK_ADDRESS: &str = "127.0.0.1:6742";

fn color(rgb: Rgb) -> Color { Color::new(rgb.0, rgb.1, rgb.2) }

/// Apply a zone's calibration gain to a color bound for the LEDs.
fn tint(color: Color, gain: Gain) -> Color {
    let [red, green, blue] = gain.apply([color.r, color.g, color.b]);
    Color::new(red, green, blue)
}

fn tint_all(colors: Vec<Color>, gain: Gain) -> Vec<Color> {
    colors.into_iter().map(|color| tint(color, gain)).collect()
}

/// The calibration for the direct-SDK renders. A bad file must never stop a
/// fault or safety render, so it degrades to unity gain with a warning.
fn direct_calibration(root: &Path, layout: &Layout) -> Calibration {
    match config::check_calibration_file(&root.join("led-calibration.toml"), layout) {
        Ok(found) => found.unwrap_or_default(),
        Err(error) => {
            eprintln!("monolithd: rendering without calibration: {error}");
            Calibration::default()
        }
    }
}

fn root() -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf() }

fn checked_level(value: usize, name: &str, maximum: usize) -> Result<usize, String> {
    if value > maximum { Err(format!("{name} must be between 0 and {maximum}")) } else { Ok(value) }
}

fn level_bar(level: usize, palette: &Palette, led_count: usize) -> Vec<Color> {
    let split = led_count.saturating_sub(level.min(led_count));
    (0..led_count).map(|index| color(if index < split { palette.primary } else { palette.secondary })).collect()
}

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

fn require_route(layout: &Layout, state: &str, required: &[&str]) -> Result<(), String> {
    let route = layout.status_routes.get(state).ok_or_else(|| format!("missing status route for {state}"))?;
    for zone in required {
        if !route.iter().any(|configured| configured == zone) {
            return Err(format!("status route {state} does not own required zone {zone}"));
        }
    }
    Ok(())
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
        let root = root();
        let layout = config::load_layout(&root.join("scene-layout.toml"))?;
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

async fn direct_mode(resolved: &Resolved<'_>) -> Result<(), String> {
    for controller in &resolved.ram { controller.set_controllable_mode().await.map_err(|error| error.to_string())?; }
    resolved.rog_eye.0.set_controllable_mode().await.map_err(|error| error.to_string())?;
    Ok(())
}

async fn set_eye(resolved: &Resolved<'_>, fill: Color, off: Color, calibration: &Calibration) -> Result<(), String> {
    let (board, zone, count) = resolved.rog_eye;
    let gain = calibration.gain("rog_eye");
    let (fill, off) = (tint(fill, gain), tint(off, gain));
    let colors = (0..count).map(|index| if index < 3 { fill } else { off }).collect::<Vec<_>>();
    board.set_zone_leds(zone, colors).await.map_err(|error| error.to_string())
}

async fn profile(root: &Path, relative: &str) -> Result<(), String> {
    let path = root.join(relative);
    if !path.is_file() { return Err(format!("profile does not exist: {}", path.display())); }
    let output = Command::new(OPENRGB).arg("--profile").arg(&path).output().await.map_err(|error| error.to_string())?;
    if output.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&output.stderr).trim().to_owned()) }
}

async fn load() -> Result<(PathBuf, Layout, PaletteFile, Vec<Controller>, Calibration), String> {
    let root = root();
    let layout = config::load_layout(&root.join("scene-layout.toml"))?;
    let palette = config::load_palette(&root.join("rgb-palette.toml"))?;
    let client = OpenRgbClient::connect_to(SDK_ADDRESS, 6).await.map_err(|error| error.to_string())?;
    let controllers = client.get_all_controllers().await.map_err(|error| error.to_string())?.into_iter().collect();
    let calibration = direct_calibration(&root, &layout);
    Ok((root, layout, palette, controllers, calibration))
}

pub async fn idle() -> Result<(), String> {
    let (root, layout, palette_file, controllers, calibration) = load().await?;
    if layout.idle_scene.kind == "profile" {
        let profile_path = layout.idle_scene.profile.as_deref().ok_or_else(|| "idle_scene.profile is required when kind = profile".to_owned())?;
        return profile(&root, profile_path).await;
    }
    if layout.idle_scene.kind != "direct" { return Err(format!("unsupported idle_scene.kind: {}", layout.idle_scene.kind)); }
    require_route(&layout, "idle", &["ram", "rog_eye"])?;
    let resolved = resolve(&layout, &controllers)?;
    direct_mode(&resolved).await?;
    for controller in &resolved.ram { controller.set_all_leds(tint(color(palette_file.colors.primary), calibration.gain("ram"))).await.map_err(|error| error.to_string())?; }
    set_eye(&resolved, color(palette_file.colors.primary), color(palette_file.colors.off), &calibration).await
}

pub async fn working(cpu: usize, gpu: usize, memory: usize, task: usize) -> Result<(), String> {
    for (name, value) in [("cpu", cpu), ("gpu", gpu), ("memory", memory), ("task", task)] { checked_level(value, name, 8)?; }
    let (_root, layout, palette_file, controllers, calibration) = load().await?;
    require_route(&layout, "working", &["ram", "rog_eye"])?;
    let resolved = resolve(&layout, &controllers)?;
    direct_mode(&resolved).await?;
    for (controller, level) in resolved.ram.iter().zip([cpu, gpu, memory, task]) {
        controller.set_leds(tint_all(level_bar(level, &palette_file.colors, controller.num_leds()), calibration.gain("ram"))).await.map_err(|error| error.to_string())?;
    }
    set_eye(&resolved, color(palette_file.colors.secondary), color(palette_file.colors.off), &calibration).await
}

pub async fn working_progress(completed: usize) -> Result<(), String> {
    checked_level(completed, "completed", 32)?;
    let (_root, layout, palette_file, controllers, calibration) = load().await?;
    require_route(&layout, "working", &["ram", "rog_eye"])?;
    let resolved = resolve(&layout, &controllers)?;
    direct_mode(&resolved).await?;
    for (position, controller) in resolved.ram.iter().enumerate() {
        let level = completed.saturating_sub(position * 8).min(8);
        controller.set_leds(tint_all(level_bar(level, &palette_file.colors, 8), calibration.gain("ram"))).await.map_err(|error| error.to_string())?;
    }
    set_eye(&resolved, color(palette_file.colors.secondary), color(palette_file.colors.off), &calibration).await
}

pub async fn warning() -> Result<(), String> {
    let (_root, layout, palette_file, controllers, calibration) = load().await?;
    require_route(&layout, "warning", &["rog_eye"])?;
    let resolved = resolve(&layout, &controllers)?;
    direct_mode(&resolved).await?;
    set_eye(&resolved, color(palette_file.colors.warning), color(palette_file.colors.off), &calibration).await
}

pub async fn fault(phase: usize) -> Result<(), String> {
    if phase > 1 { return Err("fault phase must be 0 or 1".to_owned()); }
    let (root, layout, palette_file, controllers, calibration) = load().await?;
    require_route(&layout, "fault", &["ram", "rog_eye"])?;
    let resolved = resolve(&layout, &controllers)?;
    if palette_file.fault_presentation.kind == "profile" {
        return profile(&root, &palette_file.fault_presentation.profile).await;
    }
    if palette_file.fault_presentation.kind != "ambulance" {
        return Err(format!("unsupported fault_presentation.kind: {}", palette_file.fault_presentation.kind));
    }
    let direct = async {
        direct_mode(&resolved).await?;
        for (position, controller) in resolved.ram.iter().enumerate() {
            let selected = if (position + phase) % 2 == 0 { palette_file.colors.fault } else { palette_file.colors.primary };
            controller.set_all_leds(tint(color(selected), calibration.gain("ram"))).await.map_err(|error| error.to_string())?;
        }
        set_eye(&resolved, color(if phase == 0 { palette_file.colors.primary } else { palette_file.colors.fault }), color(palette_file.colors.off), &calibration).await
    }.await;
    match direct { Ok(()) => Ok(()), Err(error) => profile(&root, &palette_file.fallbacks.fault).await.map_err(|fallback| format!("direct fault failed: {error}; fallback failed: {fallback}")) }
}

pub async fn quiet() -> Result<(), String> {
    let (root, _layout, palette_file, _controllers, _calibration) = load().await?;
    profile(&root, &palette_file.fallbacks.quiet).await
}

pub async fn controller_fault() -> Result<(), String> {
    let (root, _layout, palette_file, _controllers, _calibration) = load().await?;
    profile(&root, &palette_file.fallbacks.controller_failure).await
}

pub fn describe() -> Result<String, String> {
    let root = root();
    let layout = config::load_layout(&root.join("scene-layout.toml"))?;
    let palette = config::load_palette(&root.join("rgb-palette.toml"))?;
    Ok(format!(
        "idle_scene.kind={} idle_scene.profile={} fault.kind={} fault.interval_ms={} controller_failure_rgb={:?}",
        layout.idle_scene.kind,
        layout.idle_scene.profile.as_deref().unwrap_or("<none>"),
        palette.fault_presentation.kind,
        palette.fault_presentation.interval_ms,
        palette.colors.controller_failure,
    ))
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

    #[test]
    fn direct_renders_apply_the_same_gain() {
        let tinted = tint(Color::new(255, 255, 255), Gain([1.0, 0.5, 0.25]));
        assert_eq!((tinted.r, tinted.g, tinted.b), (255, 128, 64));
        let bar = tint_all(vec![Color::new(0, 255, 0), Color::new(255, 255, 255)], Gain([0.5, 1.0, 1.0]));
        assert_eq!(bar.iter().map(|c| (c.r, c.g, c.b)).collect::<Vec<_>>(), vec![(0, 255, 0), (128, 255, 255)]);
        let unchanged = tint(Color::new(10, 20, 30), Gain::UNITY);
        assert_eq!((unchanged.r, unchanged.g, unchanged.b), (10, 20, 30));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Rgb;

    #[test]
    fn progress_bar_grows_from_the_bottom() {
        let palette = Palette { off: Rgb(0, 0, 0), primary: Rgb(1, 1, 1), secondary: Rgb(2, 2, 2), warning: Rgb(3, 3, 3), fault: Rgb(4, 4, 4), controller_failure: Rgb(5, 5, 5) };
        assert_eq!(level_bar(3, &palette, 8), vec![color(palette.primary); 5].into_iter().chain(vec![color(palette.secondary); 3]).collect::<Vec<_>>());
    }

    #[test]
    fn rejects_invalid_levels() { assert!(checked_level(9, "cpu", 8).is_err()); }
}
