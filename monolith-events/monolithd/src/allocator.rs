//! Job allocation: which truthful job owns which progress zone, and what each zone should show.
//!
//! Pure state with no I/O, so the policy is easy to test. The rules:
//! - a job leased a zone keeps it until it finishes or fails, so a visible lease is never reshuffled;
//! - a free zone goes to the highest-priority queued job, earliest first;
//! - a completed job holds its zone at 100% for a while, then releases it;
//! - progress is `completed / total` of the zone's step count, never inferred from utilization.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const MAX_FAILURES: usize = 8;

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
}

pub struct Planner {
    /// Progress-capable zones in allocation order, with their step counts.
    zones: Vec<(String, u32)>,
    jobs: Vec<Job>,
    next_seq: u64,
    hold: Duration,
    failures: VecDeque<FailedJob>,
}

impl Planner {
    pub fn new(zones: Vec<(String, u32)>, hold: Duration) -> Self {
        Self { zones, jobs: Vec::new(), next_seq: 0, hold, failures: VecDeque::new() }
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

    fn assign(&mut self) {
        for (zone, _) in self.zones.clone() {
            if self.jobs.iter().any(|job| job.state.zone() == Some(zone.as_str())) {
                continue;
            }
            let best = self
                .jobs
                .iter()
                .enumerate()
                .filter(|(_, job)| job.state == JobState::Queued)
                .max_by(|(_, a), (_, b)| a.priority.cmp(&b.priority).then(b.seq.cmp(&a.seq)))
                .map(|(index, _)| index);
            if let Some(index) = best {
                self.jobs[index].state = JobState::Leased(zone);
            }
        }
    }

    pub fn target(&self, zone: &str) -> ZoneTarget {
        let Some((_, steps)) = self.zones.iter().find(|(name, _)| name == zone) else { return ZoneTarget::Ambient };
        match self.jobs.iter().find(|job| job.state.zone() == Some(zone)) {
            Some(job) => {
                let fraction = f64::from(job.completed) / f64::from(job.total);
                ZoneTarget::Progress { step: ((fraction * f64::from(*steps)).round() as u32).min(*steps), pattern: job.pattern.clone() }
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
}
