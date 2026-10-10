//! The events waiting for a coordinator, and where each stands. An event is
//! pending until a coordinator takes it, in progress while that coordinator
//! handles it, and done once handled. A coordinator that ends with events in
//! progress gives them back: they are pending again for the next one. One
//! coordinator runs at a time, so the events in progress are always the
//! current holder's.
//!
//! A run `watch` started has handled its events when it exits successfully.
//! An interactive coordinator has handled a batch when it asks for the next
//! one. An event given back by `MAX_FAILURES` failed runs is closed rather
//! than retried forever.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use watch::events::{self, Event};

/// Failed runs after which an event is closed.
pub const MAX_FAILURES: u32 = 3;
/// Done events kept in the queue, the latest, to show what was handled.
const KEEP_DONE: usize = 20;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Pending,
    InProgress,
    Done,
}

/// An event in the queue. Pending events of the same key merge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Queued {
    pub key: String,
    /// How many events merged into this one.
    pub count: u32,
    /// When the first of them came, in seconds since the Unix epoch.
    pub first_at: u64,
    /// The latest of them, as the events file holds it.
    pub event: Value,
    #[serde(default)]
    pub status: Status,
    /// Runs that took it and failed.
    #[serde(default)]
    pub failures: u32,
}

impl Queued {
    /// The event `watch` wrote at `at`, if it needs judgment. Memory pressure
    /// events all merge, the latest one standing for the others; an
    /// admission wait is reported once per call. Orphans are not the
    /// coordinator's yet.
    pub fn new(at: u64, event: &Event) -> Result<Option<Queued>> {
        let key = match event {
            Event::MemoryPressure(watch::events::MemoryPressure { .. }) => {
                "memory_pressure".to_string()
            }
            Event::AdmissionWait(watch::events::AdmissionWait { job, .. }) => {
                format!("admission_wait {job}")
            }
            Event::Orphans(watch::events::Orphans { .. }) => return Ok(None),
        };
        Ok(Some(Queued {
            key,
            count: 1,
            first_at: at,
            event: events::to_value(at, event)?,
            status: Status::Pending,
            failures: 0,
        }))
    }

    /// The call an admission wait is about.
    fn waiting_job(&self) -> Option<&str> {
        (self.event["kind"] == "admission_wait")
            .then(|| self.event["job"].as_str())
            .flatten()
    }

    /// What the coordinator reads: the event, with how many merged into it
    /// and when the first came.
    pub fn line(&self) -> String {
        let mut event = self.event.clone();
        if let Value::Object(fields) = &mut event {
            fields.insert("count".into(), self.count.into());
            fields.insert("first_at".into(), self.first_at.into());
        }
        event.to_string()
    }
}

/// Adds `new` to `queue`: a pending event of the same key takes its content
/// and keeps its place. An event already taken is not changed.
pub fn merge(queue: &mut Vec<Queued>, new: Queued) {
    let same = queue
        .iter_mut()
        .find(|q| q.key == new.key && q.status == Status::Pending);
    match same {
        Some(q) => {
            q.count = q.count.saturating_add(new.count);
            q.first_at = q.first_at.min(new.first_at);
            q.event = new.event;
        }
        None => queue.push(new),
    }
}

/// Takes the pending events, now in progress, given the jobs of the calls
/// waiting now. An admission wait whose call runs already needs nothing: it
/// is done without being taken.
pub fn take(queue: &mut [Queued], waiting: &[String]) -> Vec<Queued> {
    let mut taken = Vec::new();
    for q in queue.iter_mut().filter(|q| q.status == Status::Pending) {
        let ended = q
            .waiting_job()
            .is_some_and(|job| !waiting.iter().any(|w| w == job));
        if ended {
            q.status = Status::Done;
        } else {
            q.status = Status::InProgress;
            taken.push(q.clone());
        }
    }
    taken
}

/// The events in progress were handled, or their run failed: then they are
/// pending again, unless they have failed `MAX_FAILURES` runs. Returns those
/// closed for that.
pub fn finish(queue: &mut [Queued], handled: bool) -> Vec<Queued> {
    let mut closed = Vec::new();
    for q in queue.iter_mut().filter(|q| q.status == Status::InProgress) {
        if handled {
            q.status = Status::Done;
            continue;
        }
        q.failures = q.failures.saturating_add(1);
        if q.failures >= MAX_FAILURES {
            q.status = Status::Done;
            closed.push(q.clone());
        } else {
            q.status = Status::Pending;
        }
    }
    closed
}

/// The coordinator holding the events in progress ended without saying they
/// were handled, through no fault of theirs: an interactive coordinator
/// closed, or a run that never started. They are pending again.
pub fn give_back(queue: &mut [Queued]) {
    for q in queue.iter_mut().filter(|q| q.status == Status::InProgress) {
        q.status = Status::Pending;
    }
}

/// Drops the oldest done events, keeping the latest `KEEP_DONE`.
pub fn prune(queue: &mut Vec<Queued>) {
    let done = queue.iter().filter(|q| q.status == Status::Done).count();
    let mut excess = done.saturating_sub(KEEP_DONE);
    queue.retain(|q| {
        if excess > 0 && q.status == Status::Done {
            excess -= 1;
            false
        } else {
            true
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use watch::events::SessionBrief;

    fn pressure(available_mb: u64) -> Event {
        Event::MemoryPressure(watch::events::MemoryPressure {
            available_mb,
            stall_ms: 200,
            largest: None,
            next: vec![SessionBrief {
                session: "alpha".into(),
                rss_mb: 4000,
            }],
        })
    }

    fn wait(job: &str) -> Event {
        Event::AdmissionWait(watch::events::AdmissionWait {
            session: Some("alpha".into()),
            session_id: Some("a".into()),
            job: job.into(),
            command: "make".into(),
            waited_secs: 20,
            peak_mb: 3000,
            need_mb: 5000,
            free_mb: 1000,
        })
    }

    pub(crate) fn queued(at: u64, event: &Event) -> Queued {
        Queued::new(at, event).unwrap().unwrap()
    }

    fn statuses(queue: &[Queued]) -> Vec<Status> {
        queue.iter().map(|q| q.status).collect()
    }

    #[test]
    fn duplicates_merge_into_the_latest() {
        let mut queue = Vec::new();
        merge(&mut queue, queued(10, &pressure(900)));
        merge(&mut queue, queued(11, &wait("job-bash-1-1")));
        merge(&mut queue, queued(70, &pressure(500)));
        merge(&mut queue, queued(71, &wait("job-bash-2-1")));
        assert_eq!(queue.len(), 3);
        assert_eq!(
            (
                queue[0].count,
                queue[0].first_at,
                &queue[0].event["available_mb"]
            ),
            (2, 10, &Value::from(500))
        );
        assert_eq!(queue[0].event["at"], 70);
        assert_eq!(queue[1].key, "admission_wait job-bash-1-1");
        assert_eq!(queue[2].key, "admission_wait job-bash-2-1");
        let line: Value = serde_json::from_str(&queue[0].line()).unwrap();
        assert_eq!(line["kind"], "memory_pressure");
        assert_eq!((&line["count"], &line["first_at"]), (&2.into(), &10.into()));
    }

    #[test]
    fn orphans_are_not_queued() {
        let orphans = Event::Orphans(watch::events::Orphans { orphans: vec![] });
        assert_eq!(Queued::new(1, &orphans).unwrap(), None);
    }

    #[test]
    fn an_event_moves_from_pending_to_in_progress_to_done() {
        let mut queue = vec![queued(1, &pressure(900))];
        assert_eq!(statuses(&queue), [Status::Pending]);
        let taken = take(&mut queue, &[]);
        assert_eq!(taken.len(), 1);
        assert_eq!(statuses(&queue), [Status::InProgress]);
        // Taken again, nothing: it is in progress.
        assert_eq!(take(&mut queue, &[]), []);
        assert_eq!(finish(&mut queue, true), []);
        assert_eq!(statuses(&queue), [Status::Done]);
    }

    #[test]
    fn a_new_event_does_not_merge_into_one_in_progress() {
        let mut queue = vec![queued(1, &pressure(900))];
        take(&mut queue, &[]);
        merge(&mut queue, queued(2, &pressure(800)));
        assert_eq!(statuses(&queue), [Status::InProgress, Status::Pending]);
        // The next coordinator gets only the new one.
        finish(&mut queue, true);
        let next = take(&mut queue, &[]);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].event["available_mb"], 800);
    }

    #[test]
    fn a_failed_run_gives_its_events_back_until_it_gives_up() {
        let mut queue = vec![queued(1, &pressure(900))];
        for failure in 1..MAX_FAILURES {
            assert_eq!(take(&mut queue, &[]).len(), 1);
            assert_eq!(finish(&mut queue, false), []);
            assert_eq!(statuses(&queue), [Status::Pending]);
            assert_eq!(queue[0].failures, failure);
        }
        take(&mut queue, &[]);
        let closed = finish(&mut queue, false);
        assert_eq!(closed.len(), 1);
        assert_eq!(statuses(&queue), [Status::Done]);
    }

    #[test]
    fn a_coordinator_that_closes_gives_its_events_back() {
        let mut queue = vec![queued(1, &pressure(900)), queued(2, &wait("job-bash-1-1"))];
        take(&mut queue, &["job-bash-1-1".to_string()]);
        give_back(&mut queue);
        assert_eq!(statuses(&queue), [Status::Pending, Status::Pending]);
        assert_eq!(queue[0].failures, 0, "not the event's failure");
    }

    #[test]
    fn a_wait_that_ended_needs_no_judgment() {
        let mut queue = vec![
            queued(1, &pressure(900)),
            queued(2, &wait("job-bash-1-1")),
            queued(3, &wait("job-bash-2-1")),
        ];
        let taken = take(&mut queue, &["job-bash-2-1".to_string()]);
        let keys: Vec<&str> = taken.iter().map(|q| q.key.as_str()).collect();
        assert_eq!(keys, ["memory_pressure", "admission_wait job-bash-2-1"]);
        assert_eq!(
            statuses(&queue),
            [Status::InProgress, Status::Done, Status::InProgress]
        );
    }

    #[test]
    fn only_the_latest_done_events_are_kept() {
        let mut queue: Vec<Queued> = (0..30)
            .map(|i| {
                let mut q = queued(i, &wait(&format!("job-bash-{i}-1")));
                q.status = Status::Done;
                q
            })
            .collect();
        queue.push(queued(99, &pressure(900)));
        prune(&mut queue);
        assert_eq!(queue.len(), KEEP_DONE + 1);
        assert_eq!(queue[0].first_at, 10, "the oldest went");
        assert_eq!(queue.last().unwrap().status, Status::Pending);
    }
}
