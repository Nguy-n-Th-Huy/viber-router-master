use deadpool_redis::Pool;
use deadpool_redis::redis::cmd;
use uuid::Uuid;

/// Consecutive probe successes required to close a half-open circuit.
pub const SUCCESS_THRESHOLD: i64 = 2;

/// Circuit state derived from Redis keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CbState {
    Closed,
    Open,
    HalfOpen,
}

/// Routing decision for a request against a circuit-breaker-enabled server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Circuit closed — normal request.
    Allow,
    /// Half-open — this request is the probe.
    Probe,
    /// Open, or half-open with a probe already in flight — skip this server.
    Skip,
}

/// Outcome of a successful request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuccessOutcome {
    /// Nothing to do (circuit closed).
    Noop,
    /// Probe succeeded but more successes needed — stay half-open.
    StayHalfOpen,
    /// Enough consecutive probe successes — close the circuit.
    Close,
}

/// Outcome of a failed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureOutcome {
    /// Below threshold — just count the error.
    Count,
    /// Threshold reached — trip the circuit open.
    Trip,
    /// Failure during half-open probe — reopen immediately.
    Reopen,
}

/// Derive the circuit state from Redis key existence.
fn derive_state(open_exists: bool, half_exists: bool) -> CbState {
    if open_exists {
        CbState::Open
    } else if half_exists {
        CbState::HalfOpen
    } else {
        CbState::Closed
    }
}

/// Decide request access from the circuit state and probe permit acquisition.
fn decide_access(state: CbState, probe_permit_acquired: bool) -> Access {
    match state {
        CbState::Closed => Access::Allow,
        CbState::Open => Access::Skip,
        CbState::HalfOpen => {
            if probe_permit_acquired {
                Access::Probe
            } else {
                Access::Skip
            }
        }
    }
}

/// Decide what a successful request does to the circuit.
fn on_success(
    state: CbState,
    consecutive_successes: i64,
    success_threshold: i64,
) -> SuccessOutcome {
    match state {
        CbState::HalfOpen => {
            if consecutive_successes >= success_threshold {
                SuccessOutcome::Close
            } else {
                SuccessOutcome::StayHalfOpen
            }
        }
        _ => SuccessOutcome::Noop,
    }
}

/// Decide what a failed request does to the circuit.
fn on_failure(state: CbState, error_count: i64, max_failures: i64) -> FailureOutcome {
    match state {
        CbState::HalfOpen => FailureOutcome::Reopen,
        _ => {
            if error_count >= max_failures {
                FailureOutcome::Trip
            } else {
                FailureOutcome::Count
            }
        }
    }
}

fn model_slot(model: Option<&str>) -> &str {
    model.unwrap_or("_any")
}

/// TTL for the in-flight probe permit — released early on probe completion,
/// this bounds how long a permit can stay stuck if an instance dies mid-probe.
const PROBE_TTL_SECONDS: i64 = 120;

/// How long the half-open marker outlives the cooldown. If no traffic arrives
/// to probe within this grace period, the marker expires and the circuit
/// silently closes (the pre-probe behavior).
const HALF_OPEN_GRACE_SECONDS: i32 = 3600;

struct CbKeys {
    err: String,
    open: String,
    half: String,
    probe: String,
    succ: String,
}

fn keys(group_id: Uuid, server_id: Uuid, model: Option<&str>) -> CbKeys {
    let slot = model_slot(model);
    CbKeys {
        err: format!("cb:err:{group_id}:{server_id}:{slot}"),
        open: format!("cb:open:{group_id}:{server_id}:{slot}"),
        half: format!("cb:half:{group_id}:{server_id}:{slot}"),
        probe: format!("cb:probe:{group_id}:{server_id}:{slot}"),
        succ: format!("cb:succ:{group_id}:{server_id}:{slot}"),
    }
}

/// Determine access for a request to this group-server-model triple.
/// In half-open state, at most one in-flight request acquires the probe
/// permit (`Access::Probe`); everyone else gets `Access::Skip`.
/// Fails open (`Access::Allow`) on Redis errors.
pub async fn check_access(
    redis: &Pool,
    group_id: Uuid,
    server_id: Uuid,
    model: Option<&str>,
) -> Access {
    let k = keys(group_id, server_id, model);
    let Ok(mut conn) = redis.get().await else {
        return Access::Allow; // fail open
    };

    let open_exists: bool = cmd("EXISTS")
        .arg(&k.open)
        .query_async(&mut conn)
        .await
        .unwrap_or(false);
    let half_exists: bool = cmd("EXISTS")
        .arg(&k.half)
        .query_async(&mut conn)
        .await
        .unwrap_or(false);

    let state = derive_state(open_exists, half_exists);

    // Only try to acquire the probe permit when half-open
    let probe_acquired = if state == CbState::HalfOpen {
        let acquired: Option<String> = cmd("SET")
            .arg(&k.probe)
            .arg("1")
            .arg("NX")
            .arg("EX")
            .arg(PROBE_TTL_SECONDS)
            .query_async(&mut conn)
            .await
            .unwrap_or(None);
        acquired.is_some()
    } else {
        false
    };

    decide_access(state, probe_acquired)
}

/// Record a successful probe request. Returns true if the circuit was newly
/// closed (enough consecutive probe successes) — the caller should send a
/// re-enable alert.
pub async fn record_probe_success(
    redis: &Pool,
    group_id: Uuid,
    server_id: Uuid,
    model: Option<&str>,
) -> bool {
    let k = keys(group_id, server_id, model);
    let Ok(mut conn) = redis.get().await else {
        return false;
    };

    let successes: i64 = match cmd("INCR").arg(&k.succ).query_async(&mut conn).await {
        Ok(c) => c,
        Err(_) => return false,
    };
    if successes == 1 {
        // Bound the counter's lifetime to the half-open grace window
        let _: Result<(), _> = cmd("EXPIRE")
            .arg(&k.succ)
            .arg(HALF_OPEN_GRACE_SECONDS)
            .query_async(&mut conn)
            .await;
    }

    match on_success(CbState::HalfOpen, successes, SUCCESS_THRESHOLD) {
        SuccessOutcome::Close => {
            // DEL half returns 1 only for the instance that actually closed it,
            // deduplicating the re-enable alert across concurrent probes.
            let closed: i64 = cmd("DEL")
                .arg(&k.half)
                .query_async(&mut conn)
                .await
                .unwrap_or(0);
            let _: Result<(), _> = cmd("DEL")
                .arg(&k.succ)
                .arg(&k.probe)
                .arg(&k.err)
                .query_async(&mut conn)
                .await;
            closed > 0
        }
        _ => {
            // Release the probe permit so the next request can probe
            let _: Result<(), _> = cmd("DEL").arg(&k.probe).query_async(&mut conn).await;
            false
        }
    }
}

/// Release the probe permit without recording success or failure.
/// Used when a probe request ends in an outcome that says nothing about
/// server health (e.g. a non-failover client error returned to the caller).
pub async fn release_probe(redis: &Pool, group_id: Uuid, server_id: Uuid, model: Option<&str>) {
    let k = keys(group_id, server_id, model);
    let Ok(mut conn) = redis.get().await else {
        return;
    };
    let _: Result<(), _> = cmd("DEL").arg(&k.probe).query_async(&mut conn).await;
}

/// Record an error for this group-server-model triple.
/// `was_probe` marks a failed half-open probe, which reopens the circuit
/// immediately without waiting for `max_failures`.
/// Returns true if the circuit was newly tripped/reopened (alert should be sent).
#[allow(clippy::too_many_arguments)]
pub async fn record_error(
    redis: &Pool,
    group_id: Uuid,
    server_id: Uuid,
    model: Option<&str>,
    max_failures: i32,
    window_seconds: i32,
    cooldown_seconds: i32,
    was_probe: bool,
) -> bool {
    let k = keys(group_id, server_id, model);

    let Ok(mut conn) = redis.get().await else {
        return false;
    };

    if was_probe {
        // Probe failed: reopen immediately, reset probe bookkeeping
        let reopened = trip_open(&mut conn, &k, cooldown_seconds).await;
        let _: Result<(), _> = cmd("DEL")
            .arg(&k.succ)
            .arg(&k.probe)
            .query_async(&mut conn)
            .await;
        return reopened;
    }

    // INCR error counter
    let count: i64 = match cmd("INCR").arg(&k.err).query_async(&mut conn).await {
        Ok(c) => c,
        Err(_) => return false,
    };

    // Set TTL if this is a new key
    if count == 1 {
        let _: Result<(), _> = cmd("EXPIRE")
            .arg(&k.err)
            .arg(window_seconds)
            .query_async(&mut conn)
            .await;
    }

    match on_failure(CbState::Closed, count, max_failures as i64) {
        FailureOutcome::Trip => {
            let tripped = trip_open(&mut conn, &k, cooldown_seconds).await;
            // Delete error counter
            let _: Result<(), _> = cmd("DEL").arg(&k.err).query_async(&mut conn).await;
            tripped
        }
        _ => false,
    }
}

/// Open the circuit and arm the half-open marker for after the cooldown.
/// Returns true if this call was the one that opened it (NX).
async fn trip_open(
    conn: &mut deadpool_redis::Connection,
    k: &CbKeys,
    cooldown_seconds: i32,
) -> bool {
    // Use NX to avoid resetting TTL if already open
    let newly_set: Option<String> = cmd("SET")
        .arg(&k.open)
        .arg("1")
        .arg("NX")
        .arg("EX")
        .arg(cooldown_seconds)
        .query_async(conn)
        .await
        .unwrap_or(None);

    if newly_set.is_some() {
        // Arm the half-open marker: it outlives the open key, so when the
        // cooldown expires the circuit lands in half-open instead of closed.
        let _: Result<(), _> = cmd("SET")
            .arg(&k.half)
            .arg("1")
            .arg("EX")
            .arg(cooldown_seconds + HALF_OPEN_GRACE_SECONDS)
            .query_async(conn)
            .await;
    }

    newly_set.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- derive_state ---

    #[test]
    fn state_is_open_when_open_key_exists() {
        assert_eq!(derive_state(true, false), CbState::Open);
    }

    #[test]
    fn state_is_open_even_if_half_marker_also_exists() {
        // open key takes precedence: during cooldown the half marker is already set
        assert_eq!(derive_state(true, true), CbState::Open);
    }

    #[test]
    fn state_is_half_open_when_only_half_marker_exists() {
        assert_eq!(derive_state(false, true), CbState::HalfOpen);
    }

    #[test]
    fn state_is_closed_when_no_keys_exist() {
        assert_eq!(derive_state(false, false), CbState::Closed);
    }

    // --- decide_access ---

    #[test]
    fn closed_circuit_allows_request() {
        assert_eq!(decide_access(CbState::Closed, false), Access::Allow);
    }

    #[test]
    fn open_circuit_skips_request() {
        assert_eq!(decide_access(CbState::Open, false), Access::Skip);
    }

    #[test]
    fn half_open_with_permit_probes() {
        assert_eq!(decide_access(CbState::HalfOpen, true), Access::Probe);
    }

    #[test]
    fn half_open_without_permit_skips() {
        // another probe is already in flight
        assert_eq!(decide_access(CbState::HalfOpen, false), Access::Skip);
    }

    // --- on_success ---

    #[test]
    fn success_on_closed_circuit_is_noop() {
        assert_eq!(on_success(CbState::Closed, 1, 2), SuccessOutcome::Noop);
    }

    #[test]
    fn probe_success_below_threshold_stays_half_open() {
        assert_eq!(
            on_success(CbState::HalfOpen, 1, 2),
            SuccessOutcome::StayHalfOpen
        );
    }

    #[test]
    fn probe_success_at_threshold_closes_circuit() {
        assert_eq!(on_success(CbState::HalfOpen, 2, 2), SuccessOutcome::Close);
    }

    #[test]
    fn probe_success_above_threshold_closes_circuit() {
        assert_eq!(on_success(CbState::HalfOpen, 3, 2), SuccessOutcome::Close);
    }

    // --- on_failure ---

    #[test]
    fn failure_below_threshold_counts() {
        assert_eq!(on_failure(CbState::Closed, 2, 3), FailureOutcome::Count);
    }

    #[test]
    fn failure_at_threshold_trips() {
        assert_eq!(on_failure(CbState::Closed, 3, 3), FailureOutcome::Trip);
    }

    #[test]
    fn failure_above_threshold_trips() {
        assert_eq!(on_failure(CbState::Closed, 4, 3), FailureOutcome::Trip);
    }

    #[test]
    fn probe_failure_reopens_immediately() {
        // half-open probe failure does not wait for max_failures
        assert_eq!(on_failure(CbState::HalfOpen, 1, 3), FailureOutcome::Reopen);
    }
}
