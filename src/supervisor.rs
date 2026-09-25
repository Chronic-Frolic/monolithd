//! Output supervision: keep the OpenRGB output alive across transient stalls.
//!
//! OpenRGB can stall for a second or more while devices re-initialise (notably at
//! resume from suspend), and the SDK client gives up on a request after one second.
//! Treating that first error as fatal took the whole lighting stack down. The
//! supervisor makes the output stage fail-operational instead:
//!
//! 1. a write is bounded, so a stalled server cannot freeze the receiver loop;
//! 2. a failed write rebuilds the OpenRGB connection at once, because a request that
//!    timed out leaves the old connection out of step (its late reply is read as the
//!    answer to the next request), so retrying on it can never succeed;
//! 3. the unwritten frame is kept and applied on the new connection, and newer frames
//!    replace it; rebuilds are spaced out so a wedged device cannot cause a storm;
//! 4. only when the output has not recovered within 30 s does the receiver give up, and
//!    the stack restart that follows is the last resort.
//!
//! Its health is published on a watch channel for the gateway's `status`, so a
//! watchdog can tell "output recovering" from "stack failed".

use std::future::Future;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::{timeout, Instant};

/// Longest one frame write may take.
const APPLY_TIMEOUT: Duration = Duration::from_secs(2);
/// Minimum time between connection rebuilds.
const RECONNECT_SPACING: Duration = Duration::from_secs(1);
/// How long the output may stay failed before the receiver gives up.
const GIVE_UP_AFTER: Duration = Duration::from_secs(30);

/// The actuator the receiver writes frames to.
pub trait Output {
    type Frame: Clone;
    type Context;
    fn apply(&self, frame: &Self::Frame, context: &Self::Context) -> impl Future<Output = Result<(), String>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputState {
    /// The first connection to OpenRGB has not been made yet.
    Starting,
    Ok,
    /// Writes are failing; the connection has been rebuilt and the frame is being retried.
    Recovering,
    /// The OpenRGB connection is being rebuilt.
    Reconnecting,
}

impl OutputState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ok => "ok",
            Self::Recovering => "recovering",
            Self::Reconnecting => "reconnecting",
        }
    }
}

/// Output health, published for the gateway's `status`.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputReport {
    pub state: OutputState,
    pub failures_in_a_row: u32,
    /// Times the connection has been rebuilt since the receiver started.
    pub reconnects: u32,
    /// The most recent write or reconnect error, kept after recovery for diagnosis.
    pub last_error: Option<String>,
}

pub fn channel() -> (watch::Sender<OutputReport>, watch::Receiver<OutputReport>) {
    watch::channel(OutputReport { state: OutputState::Starting, failures_in_a_row: 0, reconnects: 0, last_error: None })
}

pub struct Supervisor<O: Output, C> {
    output: O,
    connect: C,
    report: watch::Sender<OutputReport>,
    pending: Option<O::Frame>,
    failing_since: Option<Instant>,
    failures: u32,
    reconnects: u32,
    last_error: Option<String>,
    state: OutputState,
    last_reconnect: Option<Instant>,
    apply_timeout: Duration,
    reconnect_spacing: Duration,
    give_up_after: Duration,
}

impl<O, C, Fut> Supervisor<O, C>
where
    O: Output,
    C: Fn() -> Fut,
    Fut: Future<Output = Result<O, String>>,
{
    pub fn new(output: O, connect: C, report: watch::Sender<OutputReport>) -> Self {
        let supervisor = Self {
            output,
            connect,
            report,
            pending: None,
            failing_since: None,
            failures: 0,
            reconnects: 0,
            last_error: None,
            state: OutputState::Ok,
            last_reconnect: None,
            apply_timeout: APPLY_TIMEOUT,
            reconnect_spacing: RECONNECT_SPACING,
            give_up_after: GIVE_UP_AFTER,
        };
        supervisor.publish();
        supervisor
    }

    #[cfg(test)]
    fn with_timings(mut self, apply_timeout: Duration, reconnect_spacing: Duration, give_up_after: Duration) -> Self {
        self.apply_timeout = apply_timeout;
        self.reconnect_spacing = reconnect_spacing;
        self.give_up_after = give_up_after;
        self
    }

    /// The newest frame becomes the one to write; an unwritten older frame is dropped.
    pub fn offer(&mut self, frame: O::Frame) {
        self.pending = Some(frame);
    }

    fn publish(&self) {
        self.report.send_replace(OutputReport {
            state: self.state,
            failures_in_a_row: self.failures,
            reconnects: self.reconnects,
            last_error: self.last_error.clone(),
        });
    }

    fn note_failure(&mut self, error: String) {
        if self.failing_since.is_none() {
            self.failing_since = Some(Instant::now());
            eprintln!("monolithd output: OpenRGB write failed ({error}); rebuilding the connection");
        }
        self.failures += 1;
        self.last_error = Some(error);
        self.state = OutputState::Recovering;
        self.publish();
    }

    fn note_success(&mut self) {
        let changed = self.state != OutputState::Ok || self.failures != 0;
        if changed {
            eprintln!("monolithd output: recovered after {} failed write(s); {} reconnect(s) since start", self.failures, self.reconnects);
        }
        self.failing_since = None;
        self.failures = 0;
        self.state = OutputState::Ok;
        if changed {
            self.publish();
        }
    }

    /// Write the pending frame, if any. Called on every output tick. Returns an
    /// error only when the output could not be re-established, which ends the receiver.
    pub async fn flush(&mut self, context: &O::Context) -> Result<(), String> {
        let Some(frame) = self.pending.clone() else { return Ok(()) };
        let error = match timeout(self.apply_timeout, self.output.apply(&frame, context)).await {
            Ok(Ok(())) => {
                self.pending = None;
                self.note_success();
                return Ok(());
            }
            Ok(Err(error)) => error,
            Err(_) => format!("write timed out after {} ms", self.apply_timeout.as_millis()),
        };
        self.note_failure(error);
        if self.failing_since.is_some_and(|since| since.elapsed() >= self.give_up_after) {
            return Err(format!(
                "OpenRGB output did not recover within {} s: {}",
                self.give_up_after.as_secs(),
                self.last_error.as_deref().unwrap_or("unknown error")
            ));
        }
        if self.last_reconnect.is_none_or(|at| at.elapsed() >= self.reconnect_spacing) {
            self.reconnect().await?;
        }
        Ok(())
    }

    async fn reconnect(&mut self) -> Result<(), String> {
        self.state = OutputState::Reconnecting;
        self.publish();
        match (self.connect)().await {
            Ok(output) => {
                self.output = output;
                self.reconnects += 1;
                self.last_reconnect = Some(Instant::now());
                self.state = OutputState::Recovering;
                self.publish();
                Ok(())
            }
            Err(error) => {
                self.last_error = Some(error.clone());
                self.publish();
                Err(format!("OpenRGB output could not be re-established: {error}"))
            }
        }
    }

    /// Rebuild the connection unconditionally, bypassing the write-failure and
    /// reconnect-spacing checks `flush` otherwise gates on: for use when something
    /// external (a resume signal) tells us the device may have silently reverted
    /// without ever failing a write. Best-effort: a failure here is logged, not
    /// propagated -- the regular write-failure path in `flush` still handles
    /// escalation (and eventually gives up) if the device is genuinely gone.
    pub async fn force_reconnect(&mut self) {
        eprintln!("monolithd output: resume signal received; proactively rebuilding the connection");
        if let Err(error) = self.reconnect().await {
            eprintln!("monolithd output: proactive reconnect after resume failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Script {
        fail: Arc<AtomicBool>,
        hang: Arc<AtomicBool>,
        connect_fails: Arc<AtomicBool>,
        connects: Arc<AtomicU32>,
        applied: Arc<Mutex<Vec<u32>>>,
    }

    struct Fake {
        script: Script,
    }

    impl Output for Fake {
        type Frame = u32;
        type Context = ();
        fn apply(&self, frame: &u32, _: &()) -> impl Future<Output = Result<(), String>> {
            let script = self.script.clone();
            let frame = *frame;
            async move {
                if script.hang.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
                if script.fail.load(Ordering::SeqCst) {
                    return Err("boom".to_owned());
                }
                script.applied.lock().unwrap().push(frame);
                Ok(())
            }
        }
    }

    const APPLY: Duration = Duration::from_millis(40);
    const SPACING: Duration = Duration::from_millis(80);
    const GIVE_UP: Duration = Duration::from_millis(400);

    type Connect = Box<dyn Fn() -> std::pin::Pin<Box<dyn Future<Output = Result<Fake, String>>>>>;

    fn supervised_with(script: &Script, spacing: Duration, give_up: Duration) -> (Supervisor<Fake, Connect>, watch::Receiver<OutputReport>) {
        let (sender, receiver) = channel();
        let connector: Connect = {
            let script = script.clone();
            Box::new(move || {
                let script = script.clone();
                Box::pin(async move {
                    script.connects.fetch_add(1, Ordering::SeqCst);
                    if script.connect_fails.load(Ordering::SeqCst) {
                        Err("no openrgb".to_owned())
                    } else {
                        Ok(Fake { script })
                    }
                })
            })
        };
        (Supervisor::new(Fake { script: script.clone() }, connector, sender).with_timings(APPLY, spacing, give_up), receiver)
    }

    fn supervised(script: &Script) -> (Supervisor<Fake, Connect>, watch::Receiver<OutputReport>) {
        supervised_with(script, SPACING, GIVE_UP)
    }

    fn applied(script: &Script) -> Vec<u32> {
        script.applied.lock().unwrap().clone()
    }

    fn connects(script: &Script) -> u32 {
        script.connects.load(Ordering::SeqCst)
    }

    #[test]
    fn a_new_supervisor_reports_ok_and_the_channel_starts_as_starting() {
        assert_eq!(channel().1.borrow().state, OutputState::Starting);
        let script = Script::default();
        let (_supervisor, report) = supervised(&script);
        let seen = report.borrow();
        assert_eq!((seen.state, seen.failures_in_a_row, seen.reconnects, seen.last_error.clone()), (OutputState::Ok, 0, 0, None));
    }

    #[tokio::test]
    async fn force_reconnect_rebuilds_the_connection_even_with_no_write_failure() {
        // The resume-signal path: nothing has failed a write, so flush's own
        // failure-triggered reconnect never fires, but force_reconnect must still
        // rebuild the connection (and, on the real Output impl, re-assert
        // direct/controllable mode) on demand.
        let script = Script::default();
        let (mut supervisor, report) = supervised(&script);
        assert_eq!(connects(&script), 0, "nothing has failed yet");
        supervisor.force_reconnect().await;
        assert_eq!(connects(&script), 1);
        assert_eq!((report.borrow().state, report.borrow().reconnects), (OutputState::Recovering, 1));
    }

    #[tokio::test]
    async fn applies_frames_in_order() {
        let script = Script::default();
        let (mut supervisor, report) = supervised(&script);
        for frame in [1, 2, 3] {
            supervisor.offer(frame);
            supervisor.flush(&()).await.unwrap();
        }
        assert_eq!(applied(&script), vec![1, 2, 3]);
        assert_eq!((report.borrow().state, connects(&script)), (OutputState::Ok, 0));
    }

    #[tokio::test]
    async fn an_idle_flush_writes_nothing_and_changes_nothing() {
        let script = Script::default();
        let (mut supervisor, report) = supervised(&script);
        supervisor.flush(&()).await.unwrap();
        assert!(applied(&script).is_empty());
        assert_eq!(report.borrow().state, OutputState::Ok);
    }

    #[tokio::test]
    async fn a_failed_write_rebuilds_the_connection_at_once_and_the_frame_survives() {
        let script = Script::default();
        let (mut supervisor, report) = supervised(&script);
        script.fail.store(true, Ordering::SeqCst);
        supervisor.offer(7);
        supervisor.flush(&()).await.unwrap();
        assert_eq!(connects(&script), 1, "no waiting: the old connection is out of step and cannot recover");
        {
            let seen = report.borrow();
            assert_eq!((seen.state, seen.failures_in_a_row, seen.reconnects), (OutputState::Recovering, 1, 1));
            assert_eq!(seen.last_error.as_deref(), Some("boom"));
        }

        script.fail.store(false, Ordering::SeqCst);
        supervisor.flush(&()).await.unwrap();
        assert_eq!(applied(&script), vec![7], "the pending frame is applied on the new connection");
        let seen = report.borrow();
        assert_eq!((seen.state, seen.failures_in_a_row, seen.reconnects), (OutputState::Ok, 0, 1));
        assert_eq!(seen.last_error.as_deref(), Some("boom"), "the last error is kept for diagnosis");
    }

    #[tokio::test]
    async fn a_newer_frame_replaces_an_unwritten_older_one() {
        let script = Script::default();
        let (mut supervisor, _report) = supervised(&script);
        script.fail.store(true, Ordering::SeqCst);
        supervisor.offer(1);
        supervisor.flush(&()).await.unwrap();
        supervisor.offer(2);
        script.fail.store(false, Ordering::SeqCst);
        supervisor.flush(&()).await.unwrap();
        assert_eq!(applied(&script), vec![2]);
    }

    #[tokio::test]
    async fn a_hung_write_is_cut_off_counted_and_triggers_a_rebuild() {
        let script = Script::default();
        let (mut supervisor, report) = supervised(&script);
        script.hang.store(true, Ordering::SeqCst);
        supervisor.offer(1);
        let started = std::time::Instant::now();
        supervisor.flush(&()).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(2), "the write must be bounded, took {:?}", started.elapsed());
        assert_eq!(connects(&script), 1);
        let seen = report.borrow();
        assert_eq!((seen.state, seen.failures_in_a_row), (OutputState::Recovering, 1));
        assert!(seen.last_error.as_deref().unwrap().contains("timed out"), "{:?}", seen.last_error);
    }

    #[tokio::test]
    async fn rebuilds_are_spaced_out_so_a_wedged_device_cannot_cause_a_storm() {
        let script = Script::default();
        let (mut supervisor, _report) = supervised(&script);
        script.fail.store(true, Ordering::SeqCst);
        supervisor.offer(1);
        supervisor.flush(&()).await.unwrap();
        supervisor.flush(&()).await.unwrap();
        supervisor.flush(&()).await.unwrap();
        assert_eq!(connects(&script), 1, "still inside the spacing window");
        tokio::time::sleep(SPACING + Duration::from_millis(30)).await;
        supervisor.flush(&()).await.unwrap();
        assert_eq!(connects(&script), 2);
    }

    #[tokio::test]
    async fn gives_up_when_the_connection_cannot_be_rebuilt() {
        let script = Script::default();
        let (mut supervisor, report) = supervised(&script);
        script.fail.store(true, Ordering::SeqCst);
        script.connect_fails.store(true, Ordering::SeqCst);
        supervisor.offer(1);
        let error = supervisor.flush(&()).await.unwrap_err();
        assert!(error.contains("could not be re-established") && error.contains("no openrgb"), "{error}");
        let seen = report.borrow();
        assert_eq!(seen.state, OutputState::Reconnecting, "the last report says what it was doing when it gave up");
        assert_eq!(seen.last_error.as_deref(), Some("no openrgb"));
    }

    #[tokio::test]
    async fn gives_up_when_reconnects_succeed_but_writes_never_recover() {
        let script = Script::default();
        let (mut supervisor, _report) = supervised_with(&script, Duration::from_millis(20), GIVE_UP);
        script.fail.store(true, Ordering::SeqCst);
        supervisor.offer(1);
        let started = std::time::Instant::now();
        let error = loop {
            if let Err(error) = supervisor.flush(&()).await {
                break error;
            }
            assert!(started.elapsed() < Duration::from_secs(3), "the supervisor never gave up");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(error.contains("did not recover"), "{error}");
        assert!(connects(&script) >= 2, "it kept trying to rebuild before giving up: {}", connects(&script));
    }
}
