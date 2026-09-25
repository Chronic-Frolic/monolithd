use crate::{config, gateway, openrgb, supervisor, watchdog};
use sacn::tokio::Receiver;
use sacn::{ReceiverConfig, ReceiverEvent, Universe};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::{interval, MissedTickBehavior};

const UNIVERSE_COUNT: usize = 5;
const UNIVERSE_SLOTS: [usize; UNIVERSE_COUNT] = [24, 24, 24, 24, 225];
const MAX_HARDWARE_FPS: u64 = 30;
/// How long to keep trying to reach OpenRGB and verify the Monolith controllers.
const OUTPUT_CONNECT_WINDOW: Duration = Duration::from_secs(30);
/// One attempt is bounded: a stalled OpenRGB can leave a connection hanging.
const OUTPUT_CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QlcFrame {
    universes: [Vec<u8>; UNIVERSE_COUNT],
}

impl QlcFrame {
    pub fn universe(&self, universe: u16) -> Result<&[u8], String> {
        let index = universe_index(universe)?;
        Ok(&self.universes[index])
    }
}

#[derive(Debug)]
struct FrameAssembler {
    universes: [Vec<u8>; UNIVERSE_COUNT],
    fresh: [bool; UNIVERSE_COUNT],
}

impl FrameAssembler {
    fn new() -> Self {
        Self {
            universes: std::array::from_fn(|index| vec![0; UNIVERSE_SLOTS[index]]),
            fresh: [false; UNIVERSE_COUNT],
        }
    }

    fn ingest(&mut self, universe: u16, slots: &[u8]) -> Result<Option<QlcFrame>, String> {
        let index = universe_index(universe)?;
        let expected_slots = UNIVERSE_SLOTS[index];
        if slots.len() < expected_slots {
            return Err(format!("QLC universe {universe} has {} DMX slots; expected at least {expected_slots}", slots.len()));
        }
        self.universes[index].copy_from_slice(&slots[..expected_slots]);
        self.fresh[index] = true;
        if self.fresh.iter().all(|fresh| *fresh) {
            self.fresh = [false; UNIVERSE_COUNT];
            Ok(Some(QlcFrame { universes: self.universes.clone() }))
        } else {
            Ok(None)
        }
    }
}

fn universe_index(universe: u16) -> Result<usize, String> {
    if !(1..=UNIVERSE_COUNT as u16).contains(&universe) {
        return Err(format!("unexpected QLC universe {universe}; expected 1 through {UNIVERSE_COUNT}"));
    }
    Ok((universe - 1) as usize)
}

fn layout_path() -> PathBuf {
    crate::paths::config_dir().join("scene-layout.toml")
}

fn accept_data(assembler: &mut FrameAssembler, latest: &mut Option<QlcFrame>, universe: u16, slots: &[u8]) -> Result<(), String> {
    if let Some(frame) = assembler.ingest(universe, slots)? {
        *latest = Some(frame);
    }
    Ok(())
}

/// Connect to OpenRGB and verify the Monolith controllers, retrying for up to 30 s.
async fn connect_output() -> Result<openrgb::QlcOutput, String> {
    let deadline = tokio::time::Instant::now() + OUTPUT_CONNECT_WINDOW;
    let last_error = loop {
        let error = match tokio::time::timeout(OUTPUT_CONNECT_ATTEMPT_TIMEOUT, openrgb::QlcOutput::connect()).await {
            Ok(Ok(output)) => return Ok(output),
            Ok(Err(error)) => error,
            Err(_) => format!("no answer from OpenRGB within {} s", OUTPUT_CONNECT_ATTEMPT_TIMEOUT.as_secs()),
        };
        if tokio::time::Instant::now() + Duration::from_millis(500) >= deadline {
            break error;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    Err(format!(
        "OpenRGB SDK did not expose the verified Monolith controllers after {} seconds: {last_error}",
        OUTPUT_CONNECT_WINDOW.as_secs()
    ))
}

pub async fn run() -> Result<(), String> {
    eprintln!("monolithd e131-receiver: {}", crate::paths::describe());
    let layout = config::load_layout(&layout_path())?;
    let listener: SocketAddr = layout.qlc_e131.listener.parse().map_err(|error| format!("parse qlc_e131.listener: {error}"))?;
    if !listener.ip().is_loopback() { return Err("qlc_e131.listener must be a loopback address".to_owned()); }

    let mut calibration = config::CalibrationWatcher::new(layout_path().with_file_name("led-calibration.toml"), &layout);
    if let Some(message) = calibration.poll() {
        eprintln!("monolithd calibration: {message}");
    }

    let (output_report, output_health) = supervisor::channel();

    gateway::spawn_for_stack(&layout, &layout_path().with_file_name("qlc-functions.toml"), calibration.subscribe(), output_health).await;

    let receiver_config = ReceiverConfig::new()
        .with_allowed_start_codes(&[0x00])
        .with_per_address_priority_handling(false)
        .with_synchronization(false)
        .with_sample_period(Duration::from_millis(100))
        .with_source_limit(1);
    let mut receiver = Receiver::bind_to(listener, receiver_config).await.map_err(|error| format!("bind {listener}: {error}"))?;
    for universe in 1..=UNIVERSE_COUNT as u16 {
        let universe = Universe::new(universe).map_err(|error| error.to_string())?;
        // QLC+ sends unicast only. Register the universe in the protocol core
        // while explicitly joining via loopback, rather than asking the sACN
        // adapter to discover a non-loopback multicast interface.
        receiver.listen_on(universe, "127.0.0.1").await.map_err(|error| format!("listen universe {universe} on loopback: {error}"))?;
    }

    let mut output = supervisor::Supervisor::new(connect_output().await?, connect_output, output_report);
    // A resume can leave a device silently back in firmware mode without ever
    // failing a write (found live 2026-09-22), so the write-failure-triggered
    // reconnect in `flush` alone can miss it. Force a reconnect on every resume
    // signal too, independent of whether anything has actually failed. Fired
    // immediately at T+0, not after a blind delay: the hardware is controllable
    // from the instant it has voltage (owner, 2026-09-22) -- the firmware's own
    // rainbow default is what shows before anything takes control, not evidence
    // it needs to "settle" -- and `force_reconnect` -> `connect_output` is
    // already the bounded, retrying, check-then-connect loop that finds out
    // when OpenRGB itself is actually ready, the same one used at cold start.
    // No timer substitutes for that.
    let (resume_tx, mut resume_rx) = tokio::sync::mpsc::channel::<()>(1);
    tokio::spawn(watchdog::watch_sleep_signal(move |sleeping| {
        let resume_tx = resume_tx.clone();
        async move {
            if !sleeping {
                let _ = resume_tx.send(()).await;
            }
        }
    }));
    let mut recalibrate = interval(Duration::from_millis(500));
    recalibrate.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut assembler = FrameAssembler::new();
    let mut latest = None;
    let mut flush = interval(Duration::from_millis(1_000 / MAX_HARDWARE_FPS));
    flush.set_missed_tick_behavior(MissedTickBehavior::Skip);
    eprintln!("monolithd E1.31 receiver: listening on {listener} for QLC universes 1-5");

    loop {
        tokio::select! {
            event = receiver.next_event() => {
                let event = event.ok_or_else(|| "E1.31 receiver stopped unexpectedly".to_owned())?;
                match event {
                    ReceiverEvent::MergedData(data) => accept_data(&mut assembler, &mut latest, data.universe.get(), data.levels())?,
                    ReceiverEvent::SyncMergedData(data) => {
                        for data in data {
                            accept_data(&mut assembler, &mut latest, data.universe.get(), data.levels())?;
                        }
                    }
                    _ => {}
                }
            }
            _ = recalibrate.tick() => {
                if let Some(message) = calibration.poll() {
                    eprintln!("monolithd calibration: {message}");
                }
            }
            _ = flush.tick() => {
                if let Some(frame) = latest.take() {
                    output.offer(frame);
                }
                output.flush(calibration.current()).await?;
            }
            Some(()) = resume_rx.recv() => output.force_reconnect().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(universe: u16, value: u8) -> Vec<u8> {
        vec![value; UNIVERSE_SLOTS[universe_index(universe).unwrap()]]
    }

    #[test]
    fn only_emits_after_a_complete_five_universe_frame() {
        let mut assembler = FrameAssembler::new();
        for universe in 1..5 {
            assert_eq!(assembler.ingest(universe, &slots(universe, universe as u8)).unwrap(), None);
        }
        let frame = assembler.ingest(5, &slots(5, 5)).unwrap().unwrap();
        assert_eq!(frame.universe(1).unwrap(), vec![1; 24]);
        assert_eq!(frame.universe(5).unwrap(), vec![5; 225]);
        assert_eq!(assembler.ingest(1, &slots(1, 9)).unwrap(), None);
    }

    #[test]
    fn rejects_unknown_or_short_universes() {
        let mut assembler = FrameAssembler::new();
        assert!(assembler.ingest(6, &[0; 1]).is_err());
        assert!(assembler.ingest(1, &[0; 23]).is_err());
    }
}
