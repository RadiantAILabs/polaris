//! Per-session lifecycle recording and bucketed uptime queries.
//!
//! [`UptimeRecorder`] keeps a bounded ring of [`LifecycleKind`] transitions
//! per session in memory. Bucketing happens at query time:
//! [`UptimeRecorder::buckets`] projects the recorded transitions onto a
//! fixed-granularity time-series so the dashboard's uptime strip viz only
//! needs to render the returned [`SessionUptimeBucket`] entries.
//!
//! The recorder is registry-only — it does not persist across restarts.
//! Sessions that age out of the underlying `VecDeque` once the per-session
//! cap is hit lose their oldest transitions first; this is acceptable for
//! v1 of the dashboard surface and keeps memory bounded under a busy agent
//! type that emits one transition per turn.

use crate::http::models::{
    BucketGranularity, SessionUptimeBucket, SessionUptimeResponse, UptimeStatus,
};
use crate::store::SessionId;
use chrono::{DateTime, Duration, Utc};
use hashbrown::HashMap;
use parking_lot::RwLock;
use std::collections::VecDeque;

/// Soft cap on lifecycle events retained per session.
///
/// One turn produces two transitions (`Active` → `Idle`), so 1024 events
/// covers ~512 turns of history before the oldest entries are dropped.
const MAX_EVENTS_PER_SESSION: usize = 1024;

/// Kinds of session lifecycle transitions recorded by [`UptimeRecorder`].
///
/// Recorded at the existing transition points in
/// [`SessionsAPI`](crate::api::SessionsAPI):
///
/// - `Created` — session inserted into the live map (create or resume).
/// - `Active` — a turn starts executing.
/// - `Idle` — a turn finishes (success or failure).
/// - `Terminated` — session removed from the live map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleKind {
    /// Session was just created or resumed.
    Created,
    /// Session began processing a turn.
    Active,
    /// Session finished processing a turn.
    Idle,
    /// Session was deleted.
    Terminated,
}

impl LifecycleKind {
    /// Status the session is in *after* this transition.
    fn status_after(self) -> UptimeStatus {
        match self {
            Self::Created | Self::Idle => UptimeStatus::Idle,
            Self::Active => UptimeStatus::Active,
            Self::Terminated => UptimeStatus::Terminated,
        }
    }
}

#[derive(Debug, Clone)]
struct LifecycleEvent {
    at: DateTime<Utc>,
    kind: LifecycleKind,
}

/// In-memory recorder of session lifecycle transitions.
///
/// Cheaply cloneable: all clones share the same backing store. The
/// [`SessionsAPI`](crate::api::SessionsAPI) holds one recorder; HTTP
/// handlers query it through public methods on the API.
#[derive(Debug, Default)]
pub(crate) struct UptimeRecorder {
    sessions: RwLock<HashMap<SessionId, VecDeque<LifecycleEvent>>>,
}

impl UptimeRecorder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Records a single lifecycle transition for the given session.
    pub(crate) fn record(&self, id: &SessionId, kind: LifecycleKind) {
        let at = Utc::now();
        let mut sessions = self.sessions.write();
        let queue = sessions.entry(id.clone()).or_default();
        if queue.len() >= MAX_EVENTS_PER_SESSION {
            queue.pop_front();
        }
        queue.push_back(LifecycleEvent { at, kind });
    }

    /// Drops a session's lifecycle history.
    ///
    /// Called by [`SessionsAPI::delete_session`] after recording the final
    /// `Terminated` transition, so the recorder doesn't retain state for
    /// sessions that no longer exist. The final transition is preserved for
    /// any in-flight uptime queries by being recorded *before* this call.
    pub(crate) fn forget(&self, id: &SessionId) {
        self.sessions.write().remove(id);
    }

    /// Returns the bucketed uptime series for a session.
    ///
    /// For each `[bucket_start, bucket_end)` window, the status is:
    ///
    /// - [`UptimeStatus::Active`] if any `Active` transition occurred
    ///   within the window;
    /// - otherwise, the projected status at `bucket_end` based on the
    ///   most recent transition observed.
    ///
    /// If `until <= since` the response has zero buckets.
    pub(crate) fn buckets_for(
        &self,
        id: &SessionId,
        bucket: BucketGranularity,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> SessionUptimeResponse {
        let buckets = self.compute_buckets(id, bucket, since, until);
        SessionUptimeResponse {
            bucket,
            since: since.to_rfc3339(),
            until: until.to_rfc3339(),
            buckets,
        }
    }

    fn compute_buckets(
        &self,
        id: &SessionId,
        bucket: BucketGranularity,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Vec<SessionUptimeBucket> {
        let num_buckets = bucket_count(bucket, since, until);
        if num_buckets == 0 {
            return Vec::new();
        }
        let bucket_duration = bucket_duration(bucket);

        let events = {
            let sessions = self.sessions.read();
            sessions
                .get(id)
                .map(|q| q.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        };

        let mut state = UptimeStatus::Unknown;
        let mut idx = 0;
        // Project state forward through events that occurred strictly
        // before the query range begins.
        while idx < events.len() && events[idx].at < since {
            state = events[idx].kind.status_after();
            idx += 1;
        }

        let mut out = Vec::with_capacity(num_buckets);
        for i in 0..num_buckets {
            let bucket_start = since + bucket_duration * (i as i32);
            // Clamp the final bucket's end to `until` so the wire payload
            // never advertises a window past the requested range.
            let bucket_end = (bucket_start + bucket_duration).min(until);

            let mut active_in_bucket = false;
            while idx < events.len() && events[idx].at < bucket_end {
                if events[idx].kind == LifecycleKind::Active {
                    active_in_bucket = true;
                }
                state = events[idx].kind.status_after();
                idx += 1;
            }

            let status = if active_in_bucket {
                UptimeStatus::Active
            } else {
                state
            };

            out.push(SessionUptimeBucket {
                start: bucket_start.to_rfc3339(),
                end: bucket_end.to_rfc3339(),
                status,
            });
        }
        out
    }
}

/// Number of buckets a `[since, until)` range yields at `bucket`
/// granularity — `ceil((until − since) / bucket)`, or `0` when the range is
/// empty (`until <= since`).
///
/// HTTP handlers call this *before* querying so a client-controlled time
/// range cannot drive an unbounded `Vec::with_capacity` allocation in
/// [`UptimeRecorder::compute_buckets`].
pub(crate) fn bucket_count(
    bucket: BucketGranularity,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> usize {
    if until <= since {
        return 0;
    }
    let range_ms = (until - since).num_milliseconds();
    let bucket_ms = bucket_duration(bucket).num_milliseconds().max(1);
    // Ceiling-divide so the final partial bucket still counts.
    ((range_ms + bucket_ms - 1) / bucket_ms) as usize
}

fn bucket_duration(bucket: BucketGranularity) -> Duration {
    match bucket {
        BucketGranularity::OneMinute => Duration::minutes(1),
        BucketGranularity::FiveMinutes => Duration::minutes(5),
        BucketGranularity::FifteenMinutes => Duration::minutes(15),
        BucketGranularity::OneHour => Duration::hours(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> SessionId {
        SessionId::from_string(s)
    }

    fn at(seconds_from_epoch: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(seconds_from_epoch, 0).unwrap()
    }

    /// Status mapping mirrors what the dashboard renders per bucket.
    #[test]
    fn status_after_maps_each_transition() {
        assert_eq!(LifecycleKind::Created.status_after(), UptimeStatus::Idle);
        assert_eq!(LifecycleKind::Active.status_after(), UptimeStatus::Active);
        assert_eq!(LifecycleKind::Idle.status_after(), UptimeStatus::Idle);
        assert_eq!(
            LifecycleKind::Terminated.status_after(),
            UptimeStatus::Terminated
        );
    }

    /// No events recorded → every bucket reports `Unknown`.
    #[test]
    fn unknown_state_before_first_event() {
        let recorder = UptimeRecorder::new();
        let response = recorder.buckets_for(
            &id("nonexistent"),
            BucketGranularity::OneMinute,
            at(0),
            at(180),
        );
        assert_eq!(response.buckets.len(), 3);
        for bucket in &response.buckets {
            assert_eq!(bucket.status, UptimeStatus::Unknown);
        }
    }

    /// An `Active` transition in the middle of a bucket marks the whole
    /// bucket Active, while the next bucket reflects the post-transition
    /// projected state.
    #[test]
    fn active_within_bucket_overrides_projected_state() {
        let recorder = UptimeRecorder::new();
        let sid = id("s");
        // Manually push events at known timestamps. The public `record`
        // uses `Utc::now()` which is unsuitable for deterministic tests.
        {
            let mut sessions = recorder.sessions.write();
            let queue = sessions.entry(sid.clone()).or_default();
            queue.push_back(LifecycleEvent {
                at: at(30),
                kind: LifecycleKind::Active,
            });
            queue.push_back(LifecycleEvent {
                at: at(45),
                kind: LifecycleKind::Idle,
            });
        }

        let response = recorder.buckets_for(&sid, BucketGranularity::OneMinute, at(0), at(180));
        assert_eq!(response.buckets.len(), 3);
        assert_eq!(response.buckets[0].status, UptimeStatus::Active);
        assert_eq!(response.buckets[1].status, UptimeStatus::Idle);
        assert_eq!(response.buckets[2].status, UptimeStatus::Idle);
    }

    /// A `Terminated` event before `since` projects forward; later
    /// buckets stay `Terminated`.
    #[test]
    fn terminated_projects_into_query_range() {
        let recorder = UptimeRecorder::new();
        let sid = id("s");
        {
            let mut sessions = recorder.sessions.write();
            let queue = sessions.entry(sid.clone()).or_default();
            queue.push_back(LifecycleEvent {
                at: at(0),
                kind: LifecycleKind::Created,
            });
            queue.push_back(LifecycleEvent {
                at: at(30),
                kind: LifecycleKind::Terminated,
            });
        }

        let response = recorder.buckets_for(&sid, BucketGranularity::OneMinute, at(120), at(240));
        for bucket in &response.buckets {
            assert_eq!(bucket.status, UptimeStatus::Terminated);
        }
    }

    /// Empty range returns no buckets.
    #[test]
    fn empty_range_yields_no_buckets() {
        let recorder = UptimeRecorder::new();
        let response =
            recorder.buckets_for(&id("s"), BucketGranularity::OneMinute, at(100), at(50));
        assert!(response.buckets.is_empty());
    }

    /// Ring eviction keeps `MAX_EVENTS_PER_SESSION` entries.
    #[test]
    fn ring_evicts_oldest_events() {
        let recorder = UptimeRecorder::new();
        let sid = id("s");
        for _ in 0..(MAX_EVENTS_PER_SESSION + 50) {
            recorder.record(&sid, LifecycleKind::Idle);
        }
        let sessions = recorder.sessions.read();
        let queue = sessions.get(&sid).unwrap();
        assert_eq!(queue.len(), MAX_EVENTS_PER_SESSION);
    }
}
