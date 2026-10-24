//! Job allocation: which truthful job owns which progress zone, and what each zone should show.
//!
//! Pure state with no I/O, so the policy is easy to test. The rules:
//! - a job leased a zone keeps it until it finishes or fails, so a visible lease is never reshuffled;
//! - a free zone goes to the highest-priority queued job, earliest first;
//! - a completed job holds its zone at 100% for a while, then releases it;
//! - progress is `completed / total` of the zone's step count, never inferred from utilization;
//! - one zone (the RAM) may be split into equal "sticks", each showing a separate job (owner,
//!   2026-10-23). A job takes the whole zone or one stick, as it asked or as the default says,
//!   and the kind of lease is fixed when it is taken. A job waiting for the whole zone holds
//!   back free sticks from lower-priority jobs, so it cannot starve.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const MAX_FAILURES: usize = 8;

/// Reserved `pattern` values that pick how a job uses the stick zone instead of naming a
/// progress family: one stick of it, or all of it with the zone's default family.
pub const STICKS: &str = "sticks";
pub const WHOLE: &str = "whole";

/// A zone split into equal sticks that each show a separate job.
#[derive(Debug, Clone, PartialEq)]
struct Sticks {
    zone: String,
    count: u32,
    /// What a job that asked for neither gets.
    default: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobState {
    Queued,
    Leased(String),
    Completing { zone: String, until: Instant },
}

impl JobState {
    fn zone(&self) -> Option<&str> {
        match self {
            Self::Queued => None,
            Self::Leased(zone) | Self::Completing { zone, .. } => Some(zone),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub label: String,
    pub total: u32,
    pub completed: u32,
    pub priority: i32,
    /// Which named progress family to render this job's zone with, if the caller
    /// asked for one instead of the zone's default (e.g. an alternate RAM fill
    /// order). Opaque here: the allocator just stores and reports the name.
    pub pattern: Option<String>,
    seq: u64,
    pub state: JobState,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FailedJob {
    pub id: String,
    pub label: String,
    pub reason: String,
}

/// What one progress zone should show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZoneTarget {
    Ambient,
    Progress { step: u32, pattern: Option<String> },
    /// The stick zone showing separate jobs: each stick's step (0 for an idle stick).
    Sticks { parts: Vec<u32> },
}

pub struct Planner {
    /// Progress-capable zones in allocation order, with their step counts.
    zones: Vec<(String, u32)>,
    jobs: Vec<Job>,
    next_seq: u64,
    hold: Duration,
    failures: VecDeque<FailedJob>,
    sticks: Option<Sticks>,
}

impl Planner {
    pub fn new(zones: Vec<(String, u32)>, hold: Duration) -> Self {
        Self { zones, jobs: Vec::new(), next_seq: 0, hold, failures: VecDeque::new(), sticks: None }
    }

    /// Split `zone` into `count` sticks. Ignored (the zone stays whole) unless the zone is a
    /// progress zone whose steps divide evenly into at least two sticks.
    pub fn with_sticks(mut self, zone: &str, count: u32, default_to_sticks: bool) -> Self {
        let divides = self.zones.iter().any(|(name, steps)| name == zone && count >= 2 && steps % count == 0);
        if divides {
            self.sticks = Some(Sticks { zone: zone.to_owned(), count, default: default_to_sticks });
        }
        self
    }

    /// The stick zone and how many sticks it has, when it is split.
    pub fn stick_zone(&self) -> Option<(&str, u32)> {
        self.sticks.as_ref().map(|sticks| (sticks.zone.as_str(), sticks.count))
    }

    pub fn jobs(&self) -> &[Job] {
        &self.jobs
    }

    pub fn failures(&self) -> &VecDeque<FailedJob> {
        &self.failures
    }

    /// Announce a job. Announcing an existing job again is harmless (adapters re-announce
    /// after a restart): its label, total and priority are updated and its lease is kept.
    pub fn start(&mut self, id: &str, label: &str, total: u32, priority: i32, pattern: Option<String>) -> Result<(), String> {
        if total == 0 {
            return Err("total must be greater than zero".to_owned());
        }
        if let Some(job) = self.jobs.iter_mut().find(|job| job.id == id) {
            if matches!(job.state, JobState::Completing { .. }) {
                return Err(format!("job {id} is already completing"));
            }
            job.label = label.to_owned();
            job.total = total;
            job.priority = priority;
            job.pattern = pattern;
            job.completed = job.completed.min(total);
        } else {
            self.jobs.push(Job { id: id.to_owned(), label: label.to_owned(), total, completed: 0, priority, pattern, seq: self.next_seq, state: JobState::Queued });
            self.next_seq += 1;
        }
        self.assign();
        Ok(())
    }

    pub fn progress(&mut self, id: &str, completed: u32, total: Option<u32>) -> Result<(), String> {
        let job = self.jobs.iter_mut().find(|job| job.id == id).ok_or_else(|| format!("unknown job {id}"))?;
        if matches!(job.state, JobState::Completing { .. }) {
            return Ok(());
        }
        if let Some(total) = total {
            if total == 0 {
                return Err("total must be greater than zero".to_owned());
            }
            job.total = total;
        }
        job.completed = completed.min(job.total);
        Ok(())
    }

    /// The job finished: it holds its zone at 100% for the hold time, then releases it.
    pub fn complete(&mut self, id: &str, now: Instant) -> Result<(), String> {
        let index = self.jobs.iter().position(|job| job.id == id).ok_or_else(|| format!("unknown job {id}"))?;
        match self.jobs[index].state.clone() {
            JobState::Queued => {
                self.jobs.remove(index);
            }
            JobState::Leased(zone) => {
                let job = &mut self.jobs[index];
                job.completed = job.total;
                job.state = JobState::Completing { zone, until: now + self.hold };
            }
            JobState::Completing { .. } => {}
        }
        Ok(())
    }

    /// The job failed: its zone is released at once and the failure is remembered.
    pub fn fail(&mut self, id: &str, reason: &str) -> Result<(), String> {
        let index = self.jobs.iter().position(|job| job.id == id).ok_or_else(|| format!("unknown job {id}"))?;
        let job = self.jobs.remove(index);
        self.failures.push_back(FailedJob { id: job.id, label: job.label, reason: reason.to_owned() });
        while self.failures.len() > MAX_FAILURES {
            self.failures.pop_front();
        }
        self.assign();
        Ok(())
    }

    /// Release zones whose completion hold has ended, then hand free zones to queued jobs.
    pub fn tick(&mut self, now: Instant) {
        self.jobs.retain(|job| !matches!(&job.state, JobState::Completing { until, .. } if *until <= now));
        self.assign();
    }

    fn lease_name(sticks: &Sticks, stick: u32) -> String {
        format!("{}.{stick}", sticks.zone)
    }

    /// Which stick of the stick zone a lease is, if it is one.
    fn stick_of(&self, lease: &str) -> Option<u32> {
        let sticks = self.sticks.as_ref()?;
        lease.strip_prefix(sticks.zone.as_str())?.strip_prefix('.')?.parse().ok().filter(|stick| *stick < sticks.count)
    }

    fn wants_sticks(&self, job: &Job) -> bool {
        self.sticks.is_some()
            && match job.pattern.as_deref() {
                Some(STICKS) => true,
                Some(_) => false,
                None => self.sticks.as_ref().is_some_and(|sticks| sticks.default),
            }
    }

    /// The leases a job may take, best first: the progress zones in order, the stick zone
    /// replaced by its sticks for a job that wants one.
    fn candidates(&self, job: &Job) -> Vec<String> {
        let wants_sticks = self.wants_sticks(job);
        let mut slots = Vec::new();
        for (zone, _) in &self.zones {
            match &self.sticks {
                Some(sticks) if wants_sticks && *zone == sticks.zone => slots.extend((0..sticks.count).map(|stick| Self::lease_name(sticks, stick))),
                _ => slots.push(zone.clone()),
            }
        }
        slots
    }

    /// Is nothing leased on this slot, or on anything that overlaps it (the whole stick zone
    /// overlaps each stick)?
    fn is_free(&self, slot: &str) -> bool {
        let stick_zone = self.sticks.as_ref().map(|sticks| sticks.zone.as_str());
        self.jobs.iter().filter_map(|job| job.state.zone()).all(|lease| {
            lease != slot
                && !(Some(slot) == stick_zone && self.stick_of(lease).is_some())
                && !(self.stick_of(slot).is_some() && Some(lease) == stick_zone)
        })
    }

    fn assign(&mut self) {
        let mut queued: Vec<usize> = (0..self.jobs.len()).filter(|&index| self.jobs[index].state == JobState::Queued).collect();
        queued.sort_by(|&a, &b| self.jobs[b].priority.cmp(&self.jobs[a].priority).then(self.jobs[a].seq.cmp(&self.jobs[b].seq)));
        // Set once a job could not get the whole stick zone because sticks are busy: it waits,
        // and no lower-priority job may take a stick until it has the zone.
        let mut whole_waiting = false;
        for index in queued {
            let slot = self
                .candidates(&self.jobs[index])
                .into_iter()
                .find(|slot| self.is_free(slot) && !(whole_waiting && self.stick_of(slot).is_some()));
            match slot {
                Some(slot) => self.jobs[index].state = JobState::Leased(slot),
                None => {
                    let any_stick_leased = self.jobs.iter().filter_map(|job| job.state.zone()).any(|lease| self.stick_of(lease).is_some());
                    if any_stick_leased && !self.wants_sticks(&self.jobs[index]) && self.sticks.is_some() {
                        whole_waiting = true;
                    }
                }
            }
        }
    }

    /// How far the job on `zone` is, 0.0 to 1.0; `None` when the zone has no job.
    pub fn fraction(&self, zone: &str) -> Option<f64> {
        let job = self.jobs.iter().find(|job| job.state.zone() == Some(zone))?;
        Some((f64::from(job.completed) / f64::from(job.total)).min(1.0))
    }

    pub fn target(&self, zone: &str) -> ZoneTarget {
        let Some((_, steps)) = self.zones.iter().find(|(name, _)| name == zone) else { return ZoneTarget::Ambient };
        if let Some(sticks) = self.sticks.as_ref().filter(|sticks| sticks.zone == zone) {
            let per_stick = steps / sticks.count;
            let parts: Vec<u32> = (0..sticks.count)
                .map(|stick| {
                    let lease = Self::lease_name(sticks, stick);
                    self.jobs.iter().find(|job| job.state.zone() == Some(lease.as_str())).map_or(0, |job| {
                        ((f64::from(job.completed) / f64::from(job.total) * f64::from(per_stick)).round() as u32).min(per_stick)
                    })
                })
                .collect();
            if self.jobs.iter().filter_map(|job| job.state.zone()).any(|lease| self.stick_of(lease).is_some()) {
                return ZoneTarget::Sticks { parts };
            }
        }
        match self.jobs.iter().find(|job| job.state.zone() == Some(zone)) {
            Some(job) => {
                let fraction = f64::from(job.completed) / f64::from(job.total);
                let pattern = job.pattern.clone().filter(|name| name != STICKS && name != WHOLE);
                ZoneTarget::Progress { step: ((fraction * f64::from(*steps)).round() as u32).min(*steps), pattern }
            }
            None => ZoneTarget::Ambient,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOLD: Duration = Duration::from_secs(15);

    fn planner() -> Planner {
        Planner::new(vec![("ram".to_owned(), 32), ("strip".to_owned(), 70)], HOLD)
    }

    fn state(planner: &Planner, id: &str) -> JobState {
        planner.jobs().iter().find(|job| job.id == id).unwrap().state.clone()
    }

    #[test]
    fn jobs_take_ram_then_strip_and_the_rest_queue() {
        let mut planner = planner();
        for id in ["a", "b", "c"] {
            planner.start(id, id, 10, 0, None).unwrap();
        }
        assert_eq!(state(&planner, "a"), JobState::Leased("ram".to_owned()));
        assert_eq!(state(&planner, "b"), JobState::Leased("strip".to_owned()));
        assert_eq!(state(&planner, "c"), JobState::Queued);
        assert_eq!(planner.target("ram"), ZoneTarget::Progress { step: 0, pattern: None });
        assert_eq!(planner.target("rog_eye"), ZoneTarget::Ambient, "the eye is never a progress zone");
    }

    #[test]
    fn a_visible_lease_is_never_reshuffled() {
        let mut planner = planner();
        let now = Instant::now();
        for id in ["a", "b", "c"] {
            planner.start(id, id, 10, 0, None).unwrap();
        }
        planner.complete("a", now).unwrap();
        planner.tick(now + HOLD - Duration::from_millis(1));
        assert_eq!(state(&planner, "a"), JobState::Completing { zone: "ram".to_owned(), until: now + HOLD }, "still holding");
        assert_eq!(state(&planner, "c"), JobState::Queued, "the zone is not free yet");
        planner.tick(now + HOLD);
        assert!(planner.jobs().iter().all(|job| job.id != "a"));
        assert_eq!(state(&planner, "c"), JobState::Leased("ram".to_owned()), "the queued job takes the freed zone");
        assert_eq!(state(&planner, "b"), JobState::Leased("strip".to_owned()), "and the other lease did not move");
    }

    #[test]
    fn a_completed_job_holds_at_full_scale() {
        let mut planner = planner();
        planner.start("a", "a", 4, 0, None).unwrap();
        planner.progress("a", 1, None).unwrap();
        assert_eq!(planner.target("ram"), ZoneTarget::Progress { step: 8, pattern: None });
        planner.complete("a", Instant::now()).unwrap();
        assert_eq!(planner.target("ram"), ZoneTarget::Progress { step: 32, pattern: None });
        planner.progress("a", 0, None).unwrap();
        assert_eq!(planner.target("ram"), ZoneTarget::Progress { step: 32, pattern: None }, "progress after completion is ignored");
    }

    #[test]
    fn free_zones_go_to_the_highest_priority_then_the_earliest() {
        let mut planner = planner();
        let now = Instant::now();
        planner.start("first", "x", 10, 0, None).unwrap();
        planner.start("second", "x", 10, 0, None).unwrap();
        planner.start("low", "x", 10, 0, None).unwrap();
        planner.start("urgent", "x", 10, 5, None).unwrap();
        planner.start("also-low", "x", 10, 0, None).unwrap();
        planner.complete("first", now).unwrap();
        planner.tick(now + HOLD);
        assert_eq!(state(&planner, "urgent"), JobState::Leased("ram".to_owned()), "priority beats arrival order");
        planner.complete("second", now).unwrap();
        planner.tick(now + HOLD);
        assert_eq!(state(&planner, "low"), JobState::Leased("strip".to_owned()), "ties go to the earlier job");
        assert_eq!(state(&planner, "also-low"), JobState::Queued);
    }

    #[test]
    fn progress_maps_completed_over_total_onto_the_zone_steps() {
        let mut planner = planner();
        planner.start("ram-job", "x", 10, 0, None).unwrap();
        planner.start("strip-job", "x", 3, 0, None).unwrap();
        planner.progress("ram-job", 5, None).unwrap();
        planner.progress("strip-job", 1, None).unwrap();
        assert_eq!(planner.target("ram"), ZoneTarget::Progress { step: 16, pattern: None });
        assert_eq!(planner.target("strip"), ZoneTarget::Progress { step: 23, pattern: None }, "70 / 3 rounds to 23");
        planner.progress("ram-job", 999, None).unwrap();
        assert_eq!(planner.target("ram"), ZoneTarget::Progress { step: 32, pattern: None }, "completed is clamped to total");
        planner.progress("ram-job", 5, Some(20)).unwrap();
        assert_eq!(planner.target("ram"), ZoneTarget::Progress { step: 8, pattern: None }, "a new total rescales the bar");
    }

    #[test]
    fn a_failed_job_releases_its_zone_at_once_and_is_remembered() {
        let mut planner = planner();
        planner.start("a", "backup", 10, 0, None).unwrap();
        planner.start("b", "x", 10, 0, None).unwrap();
        planner.start("c", "x", 10, 0, None).unwrap();
        planner.fail("a", "disk full").unwrap();
        assert_eq!(state(&planner, "c"), JobState::Leased("ram".to_owned()));
        assert_eq!(planner.failures().back(), Some(&FailedJob { id: "a".to_owned(), label: "backup".to_owned(), reason: "disk full".to_owned() }));
    }

    #[test]
    fn only_the_latest_failures_are_kept() {
        let mut planner = planner();
        for index in 0..12 {
            let id = format!("job{index}");
            planner.start(&id, "x", 1, 0, None).unwrap();
            planner.fail(&id, "boom").unwrap();
        }
        assert_eq!(planner.failures().len(), MAX_FAILURES);
        assert_eq!(planner.failures().front().unwrap().id, "job4");
    }

    #[test]
    fn announcing_a_job_again_keeps_its_lease_and_updates_it() {
        let mut planner = planner();
        planner.start("a", "old", 10, 0, None).unwrap();
        planner.progress("a", 8, None).unwrap();
        planner.start("a", "new", 5, 3, None).unwrap();
        let job = &planner.jobs()[0];
        assert_eq!((job.label.as_str(), job.total, job.completed, job.priority), ("new", 5, 5, 3));
        assert_eq!(job.state, JobState::Leased("ram".to_owned()));
        assert_eq!(planner.jobs().len(), 1);
    }

    #[test]
    fn a_queued_job_that_completes_just_disappears() {
        let mut planner = planner();
        for id in ["a", "b", "c"] {
            planner.start(id, "x", 10, 0, None).unwrap();
        }
        planner.complete("c", Instant::now()).unwrap();
        assert_eq!(planner.jobs().len(), 2);
    }

    #[test]
    fn bad_requests_are_rejected() {
        let mut planner = planner();
        assert!(planner.start("a", "x", 0, 0, None).is_err(), "a job needs a truthful total");
        planner.start("a", "x", 10, 0, None).unwrap();
        assert!(planner.progress("nope", 1, None).is_err());
        assert!(planner.progress("a", 1, Some(0)).is_err());
        assert!(planner.complete("nope", Instant::now()).is_err());
        assert!(planner.fail("nope", "x").is_err());
        planner.complete("a", Instant::now()).unwrap();
        assert!(planner.start("a", "x", 10, 0, None).is_err(), "cannot re-announce a completing job");
        assert!(planner.complete("a", Instant::now()).is_ok(), "completing twice is harmless");
    }

    fn sticks_planner(default_to_sticks: bool) -> Planner {
        planner().with_sticks("ram", 4, default_to_sticks)
    }

    fn lease(planner: &Planner, id: &str) -> String {
        match state(planner, id) {
            JobState::Leased(zone) => zone,
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn four_jobs_take_four_sticks_then_the_strip_and_the_sixth_waits() {
        let mut planner = sticks_planner(true);
        for id in ["a", "b", "c", "d", "e", "f"] {
            planner.start(id, id, 8, 0, None).unwrap();
        }
        let leases: Vec<String> = ["a", "b", "c", "d", "e"].iter().map(|id| lease(&planner, id)).collect();
        assert_eq!(leases, ["ram.0", "ram.1", "ram.2", "ram.3", "strip"]);
        assert_eq!(state(&planner, "f"), JobState::Queued);
        planner.progress("b", 4, None).unwrap();
        planner.progress("d", 8, None).unwrap();
        assert_eq!(planner.target("ram"), ZoneTarget::Sticks { parts: vec![0, 4, 0, 8] }, "each stick shows its own job, 8 steps wide");
    }

    #[test]
    fn a_stick_is_released_alone_and_idle_sticks_show_zero() {
        let mut planner = sticks_planner(true);
        let now = Instant::now();
        planner.start("a", "a", 10, 0, None).unwrap();
        planner.start("b", "b", 10, 0, None).unwrap();
        planner.progress("a", 5, None).unwrap();
        planner.progress("b", 10, None).unwrap();
        assert_eq!(planner.target("ram"), ZoneTarget::Sticks { parts: vec![4, 8, 0, 0] }, "5 of 10 over 8 steps rounds to 4");
        planner.complete("b", now).unwrap();
        planner.start("c", "c", 10, 0, None).unwrap();
        assert_eq!(lease(&planner, "c"), "ram.2", "the held stick is not reshuffled");
        planner.tick(now + HOLD);
        planner.start("d", "d", 10, 0, None).unwrap();
        assert_eq!(lease(&planner, "d"), "ram.1", "the freed stick is reused");
        assert_eq!(lease(&planner, "a"), "ram.0");
        planner.fail("a", "x").unwrap();
        assert_eq!(planner.target("ram"), ZoneTarget::Sticks { parts: vec![0, 0, 0, 0] }, "c and d still hold sticks, at 0");
        planner.fail("c", "x").unwrap();
        planner.fail("d", "x").unwrap();
        assert_eq!(planner.target("ram"), ZoneTarget::Ambient, "no stick is leased any more");
    }

    #[test]
    fn the_default_picks_the_mode_and_each_job_can_ask_for_the_other() {
        let mut whole = sticks_planner(false);
        whole.start("plain", "x", 10, 0, None).unwrap();
        whole.start("stick", "x", 10, 0, Some(STICKS.to_owned())).unwrap();
        assert_eq!((lease(&whole, "plain"), lease(&whole, "stick")), ("ram".to_owned(), "strip".to_owned()), "the whole RAM is busy, so a stick job takes the strip");
        let mut sticks = sticks_planner(true);
        sticks.start("plain", "x", 10, 0, None).unwrap();
        sticks.start("quarters", "x", 10, 0, Some("progress_ram_quarters".to_owned())).unwrap();
        sticks.start("whole", "x", 10, 0, Some(WHOLE.to_owned())).unwrap();
        assert_eq!(lease(&sticks, "plain"), "ram.0");
        assert_eq!(lease(&sticks, "quarters"), "strip", "a named whole-RAM family wants the whole zone, which has a stick busy");
        assert_eq!(state(&sticks, "whole"), JobState::Queued);
    }

    #[test]
    fn a_whole_ram_job_and_sticks_exclude_each_other() {
        let mut planner = sticks_planner(false);
        planner.start("big", "x", 10, 0, None).unwrap();
        planner.start("s1", "x", 10, 0, Some(STICKS.to_owned())).unwrap();
        planner.start("s2", "x", 10, 0, Some(STICKS.to_owned())).unwrap();
        assert_eq!(lease(&planner, "big"), "ram");
        assert_eq!(lease(&planner, "s1"), "strip");
        assert_eq!(state(&planner, "s2"), JobState::Queued, "no stick while the whole RAM is leased");
        assert_eq!(planner.target("ram"), ZoneTarget::Progress { step: 0, pattern: None });
        let now = Instant::now();
        planner.complete("big", now).unwrap();
        planner.tick(now + HOLD);
        assert_eq!(lease(&planner, "s2"), "ram.0");
    }

    #[test]
    fn a_job_waiting_for_the_whole_ram_is_not_starved_by_later_stick_jobs() {
        let mut planner = sticks_planner(true);
        let now = Instant::now();
        planner.start("s1", "x", 10, 0, None).unwrap();
        planner.start("s2", "x", 10, 0, None).unwrap();
        planner.start("fill-strip", "x", 10, 9, Some(WHOLE.to_owned())).unwrap();
        planner.start("big", "x", 10, 5, Some(WHOLE.to_owned())).unwrap();
        planner.start("late", "x", 10, 0, None).unwrap();
        assert_eq!(lease(&planner, "fill-strip"), "strip");
        assert_eq!(state(&planner, "big"), JobState::Queued, "waits for the sticks to drain");
        assert_eq!(state(&planner, "late"), JobState::Queued, "free sticks are held back for it");
        planner.complete("s1", now).unwrap();
        planner.complete("s2", now).unwrap();
        planner.tick(now + HOLD);
        assert_eq!(lease(&planner, "big"), "ram", "the whole RAM goes to it, not to the later stick job");
        assert_eq!(state(&planner, "late"), JobState::Queued);
    }

    #[test]
    fn a_lease_keeps_its_kind_when_the_job_is_announced_again_with_another_pattern() {
        let mut planner = sticks_planner(true);
        planner.start("a", "x", 10, 0, None).unwrap();
        planner.progress("a", 5, None).unwrap();
        planner.start("a", "x", 10, 0, Some(WHOLE.to_owned())).unwrap();
        assert_eq!(lease(&planner, "a"), "ram.0");
        assert_eq!(planner.target("ram"), ZoneTarget::Sticks { parts: vec![4, 0, 0, 0] });
    }

    #[test]
    fn sticks_are_ignored_unless_the_zone_divides_evenly() {
        let mut planner = planner().with_sticks("ram", 5, true).with_sticks("nowhere", 4, true);
        assert_eq!(planner.stick_zone(), None);
        planner.start("a", "x", 10, 0, None).unwrap();
        assert_eq!(lease(&planner, "a"), "ram");
        assert_eq!(sticks_planner(true).stick_zone(), Some(("ram", 4)));
    }
}
