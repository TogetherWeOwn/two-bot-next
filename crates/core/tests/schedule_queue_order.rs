//! Hermetic due-queue ordering acceptance for the scheduled-message ticker
//! (parity §4 15 s `next_run_at` queue, §8 legacy `claimDueScheduled` /
//! `markScheduledRun`). Pins the public pure API in `two_bot_core::scheduled`
//! only: no database, clock or Discord. `Queue` is a test model of the store's
//! SQL predicates (`enabled AND next_run_at <= now ORDER BY next_run_at, id`
//! over TEXT ISO timestamps); every timestamp it compares comes from
//! `format_iso_ms`. See docs/schedule-queue-order.md.

use two_bot_core::scheduled::{
    advance_next_run_iso, advance_next_run_ms, clamp_retry_delay_ms, format_iso_ms, lease_until_ms,
    no_unique_match_text, parse_iso_ms, post_failure_retryable, resolve_scheduled_id,
    schedule_list_line, schedule_list_text, IdResolution, CLAIM_LEASE_MS, RETRY_DEFAULT_MS,
    RETRY_MAX_MS, RETRY_MIN_MS, SCHEDULER_TICK_MS, TICKER_BATCH_LIMIT,
};

/// 2026-01-01T00:00:00.000Z.
const T0: u64 = 1_767_225_600_000;
const HOUR_MS: u64 = 3_600_000;

#[derive(Debug, Clone)]
struct Row {
    id: String,
    next_run_at: String,
    interval_seconds: Option<i64>,
    enabled: bool,
}

/// How a claimed occurrence's post ended.
#[derive(Debug, Clone, Copy)]
enum Post {
    Ok,
    Failed {
        status: Option<u16>,
        retry_after_ms: Option<u64>,
    },
}

#[derive(Debug, Default)]
struct Queue {
    rows: Vec<Row>,
}

impl Queue {
    fn put(&mut self, id: &str, next_run_at_ms: u64, interval_seconds: Option<i64>) {
        self.rows.push(Row {
            id: id.to_owned(),
            next_run_at: format_iso_ms(next_run_at_ms),
            interval_seconds,
            enabled: true,
        });
    }

    fn row(&self, id: &str) -> &Row {
        self.rows.iter().find(|r| r.id == id).expect("row exists")
    }

    fn row_mut(&mut self, id: &str) -> &mut Row {
        self.rows
            .iter_mut()
            .find(|r| r.id == id)
            .expect("row exists")
    }

    /// `list_scheduled`: `ORDER BY next_run_at` on the TEXT column.
    fn list(&self) -> Vec<&Row> {
        let mut rows: Vec<&Row> = self.rows.iter().collect();
        rows.sort_by(|a, b| a.next_run_at.as_str().cmp(b.next_run_at.as_str()));
        rows
    }

    fn is_due(&self, id: &str, now_ms: u64) -> bool {
        let row = self.row(id);
        row.enabled && row.next_run_at <= format_iso_ms(now_ms)
    }

    /// `claim_due`: lease the earliest due row (`next_run_at, id`) by
    /// parking it at the lease horizon.
    fn claim(&mut self, now_ms: u64) -> Option<String> {
        let now = format_iso_ms(now_ms);
        let id = self
            .rows
            .iter()
            .filter(|r| r.enabled && r.next_run_at <= now)
            .min_by(|a, b| (&a.next_run_at, &a.id).cmp(&(&b.next_run_at, &b.id)))?
            .id
            .clone();
        self.row_mut(&id).next_run_at = format_iso_ms(lease_until_ms(now_ms));
        Some(id)
    }

    /// `complete_run`: recurring rows advance from the run instant, one-shots
    /// disable.
    fn complete(&mut self, id: &str, ran_at_ms: u64) {
        let row = self.row_mut(id);
        match row.interval_seconds {
            Some(interval) => {
                row.next_run_at = advance_next_run_iso(&format_iso_ms(ran_at_ms), interval)
                    .expect("writer-shaped run instant parses");
            }
            None => row.enabled = false,
        }
    }

    /// `retry_scheduled` for a retryable failure; a permanent failure
    /// advances/disables like a run so a dead row cannot wedge the queue.
    fn fail(&mut self, id: &str, now_ms: u64, status: Option<u16>, retry_after_ms: Option<u64>) {
        if post_failure_retryable(status) {
            let retry_at_ms = now_ms.saturating_add(clamp_retry_delay_ms(retry_after_ms));
            self.row_mut(id).next_run_at = format_iso_ms(retry_at_ms);
        } else {
            self.complete(id, now_ms);
        }
    }

    /// One ticker pass: claim and post up to `TICKER_BATCH_LIMIT` occurrences.
    fn tick(&mut self, now_ms: u64, mut post: impl FnMut(&str) -> Post) -> Vec<String> {
        let mut posted = Vec::new();
        for _ in 0..TICKER_BATCH_LIMIT {
            let Some(id) = self.claim(now_ms) else {
                break;
            };
            match post(&id) {
                Post::Ok => {
                    self.complete(&id, now_ms);
                    posted.push(id);
                }
                Post::Failed {
                    status,
                    retry_after_ms,
                } => self.fail(&id, now_ms, status, retry_after_ms),
            }
        }
        posted
    }
}

fn ok(_: &str) -> Post {
    Post::Ok
}

// ---- (1) /schedule-list orders by next_run_at ascending ----

#[test]
fn schedule_list_rows_order_by_next_run_at_ascending() {
    // Inserted out of order, ids deliberately unrelated to time order, and
    // instants straddling millisecond carries and day/month/year rollovers.
    let instants = [
        ("e", T0 + HOUR_MS),
        ("a", T0 + 1_000),
        ("d", T0 - 1),            // 2025-12-31T23:59:59.999Z
        ("b", T0 + 999),          // carry into the next second sorts after
        ("f", T0 + 59_999),       // ...and before the next minute
        ("c", 1_709_164_800_000), // 2024-02-29T00:00:00.000Z
        ("g", 4_102_444_800_000), // 2100-01-01T00:00:00.000Z
        ("h", T0),
    ];
    let mut queue = Queue::default();
    for (id, ms) in instants {
        queue.put(id, ms, Some(3_600));
    }

    let listed = queue.list();
    let listed_ms: Vec<u64> = listed
        .iter()
        .map(|r| parse_iso_ms(&r.next_run_at).expect("writer shape parses"))
        .collect();
    let mut expected_ms: Vec<u64> = instants.iter().map(|(_, ms)| *ms).collect();
    expected_ms.sort_unstable();
    assert_eq!(
        listed_ms, expected_ms,
        "lexicographic TEXT order must equal instant order"
    );
    assert!(listed.iter().all(|r| r.next_run_at.len() == 24));

    let ids: Vec<&str> = listed.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["c", "d", "h", "b", "a", "f", "e", "g"]);

    let lines: Vec<String> = listed
        .iter()
        .map(|r| {
            schedule_list_line(
                &r.id,
                "chan1",
                &r.next_run_at,
                r.interval_seconds,
                r.enabled,
            )
        })
        .collect();
    let text = schedule_list_text(&lines);
    let rendered: Vec<&str> = text.lines().collect();
    assert_eq!(rendered.len(), ids.len());
    for (line, id) in rendered.iter().zip(&ids) {
        assert!(line.starts_with(&format!("`{id}` ")), "{line}");
    }
    assert_eq!(schedule_list_text(&[]), "Nothing scheduled.");
}

#[test]
fn tick_claims_earliest_due_first_ties_by_id_and_caps_at_batch_limit() {
    let mut queue = Queue::default();
    // Twelve due rows: two share an instant (tie broken by id), inserted in
    // reverse; one future row stays untouched.
    for n in (0..12u64).rev() {
        queue.put(&format!("r{n:02}"), T0 - 12_000 + n * 1_000, None);
    }
    queue.put("tie", T0 - 12_000, None);
    queue.put("future", T0 + 1, None);

    let first = queue.tick(T0, ok);
    assert_eq!(first.len() as i64, TICKER_BATCH_LIMIT);
    assert_eq!(
        first,
        ["r00", "tie", "r01", "r02", "r03", "r04", "r05", "r06", "r07", "r08"]
    );
    let second = queue.tick(T0 + SCHEDULER_TICK_MS, ok);
    assert_eq!(second, ["r09", "r10", "r11", "future"]);
    assert!(queue.tick(T0 + 2 * SCHEDULER_TICK_MS, ok).is_empty());
}

// ---- (2) advance_next_run_ms: from run time, no catch-up burst ----

#[test]
fn recurring_row_advances_from_run_time_without_catch_up_burst() {
    let interval_seconds = 3_600;
    let mut queue = Queue::default();
    queue.put("hourly", T0, Some(interval_seconds));

    // Down for five intervals and change; the row is five occurrences behind.
    let resume = T0 + 5 * HOUR_MS + 7 * 60_000;
    let mut posts = Vec::new();
    let mut now = resume;
    while now < resume + HOUR_MS {
        if !queue.tick(now, ok).is_empty() {
            posts.push(now);
        }
        now += SCHEDULER_TICK_MS;
    }
    assert_eq!(posts, [resume], "exactly one post for the missed backlog");

    let next = &queue.row("hourly").next_run_at;
    assert_eq!(
        parse_iso_ms(next),
        Some(advance_next_run_ms(resume, interval_seconds))
    );
    assert_eq!(*next, format_iso_ms(resume + HOUR_MS));
    assert_ne!(
        *next,
        format_iso_ms(T0 + HOUR_MS),
        "advancing from the old schedule would leave the row overdue"
    );
    assert!(!queue.is_due("hourly", resume + HOUR_MS - 1));
    assert_eq!(queue.tick(resume + HOUR_MS, ok), ["hourly"]);
}

#[test]
fn one_shot_disables_after_its_run() {
    let mut queue = Queue::default();
    queue.put("once", T0, None);
    assert_eq!(queue.tick(T0, ok), ["once"]);
    assert!(!queue.row("once").enabled);
    assert!(queue.tick(T0 + 365 * 24 * HOUR_MS, ok).is_empty());
}

// ---- (3) lease_until_ms parks claimed rows past the horizon ----

#[test]
fn claimed_row_is_parked_past_the_horizon_so_a_restart_sees_it_not_due() {
    let horizon = lease_until_ms(T0) - T0;
    assert_eq!(horizon, CLAIM_LEASE_MS);
    assert!(
        horizon > SCHEDULER_TICK_MS,
        "a lease must outlive at least one restarted tick"
    );
    assert_eq!(lease_until_ms(u64::MAX), u64::MAX, "saturates, never wraps");

    let mut queue = Queue::default();
    queue.put("crashed", T0 - 2, Some(3_600));
    queue.put("other", T0 - 1, Some(3_600));

    // Process A claims, then dies before recording the run.
    assert_eq!(queue.claim(T0).as_deref(), Some("crashed"));
    assert_eq!(
        queue.row("crashed").next_run_at,
        format_iso_ms(T0 + CLAIM_LEASE_MS)
    );

    // Restarted process B: every tick inside the lease skips the parked row
    // but still drains the rest of the queue.
    let mut b_posts = Vec::new();
    let mut now = T0 + SCHEDULER_TICK_MS;
    while now < T0 + CLAIM_LEASE_MS {
        assert!(!queue.is_due("crashed", now), "parked row due at {now}");
        b_posts.extend(queue.tick(now, ok));
        now += SCHEDULER_TICK_MS;
    }
    assert_eq!(b_posts, ["other"]);
    assert!(!queue.is_due("crashed", T0 + CLAIM_LEASE_MS - 1));

    // Lease expiry is the recovery path: the row is reclaimed exactly once.
    assert_eq!(queue.tick(T0 + CLAIM_LEASE_MS, ok), ["crashed"]);
    assert!(queue
        .tick(T0 + CLAIM_LEASE_MS + SCHEDULER_TICK_MS, ok)
        .is_empty());
}

// ---- (4) retries are bounded and never wedge the queue ----

#[test]
fn retry_classification_and_delay_are_bounded() {
    assert!(post_failure_retryable(None), "no response retries");
    for status in 400..500 {
        assert_eq!(
            post_failure_retryable(Some(status)),
            status == 429,
            "{status}"
        );
    }
    for status in 500..600 {
        assert!(post_failure_retryable(Some(status)), "{status}");
    }

    assert_eq!(clamp_retry_delay_ms(None), RETRY_DEFAULT_MS);
    for hint in [
        Some(0),
        Some(1),
        Some(RETRY_MIN_MS - 1),
        Some(RETRY_MIN_MS),
        Some(45_000),
        Some(RETRY_MAX_MS),
        Some(RETRY_MAX_MS + 1),
        Some(u64::MAX),
        None,
    ] {
        let delay = clamp_retry_delay_ms(hint);
        assert!((RETRY_MIN_MS..=RETRY_MAX_MS).contains(&delay), "{hint:?}");
    }
    assert_eq!(clamp_retry_delay_ms(Some(0)), RETRY_MIN_MS);
    assert_eq!(clamp_retry_delay_ms(Some(u64::MAX)), RETRY_MAX_MS);
}

#[test]
fn retryable_head_is_requeued_inside_the_window_and_the_tick_moves_on() {
    let mut queue = Queue::default();
    queue.put("flaky", T0 - 3, Some(3_600));
    queue.put("hostile", T0 - 2, Some(3_600));
    queue.put("healthy", T0 - 1, Some(3_600));

    let posted = queue.tick(T0, |id| match id {
        "flaky" => Post::Failed {
            status: Some(503),
            retry_after_ms: Some(0),
        },
        "hostile" => Post::Failed {
            status: Some(429),
            retry_after_ms: Some(u64::MAX),
        },
        _ => Post::Ok,
    });
    assert_eq!(posted, ["healthy"], "failing heads did not block the tick");

    // A zero hint still waits the floor, so the same tick never spins on it.
    assert_eq!(
        queue.row("flaky").next_run_at,
        format_iso_ms(T0 + RETRY_MIN_MS)
    );
    // A hostile hint is capped, so the row is retried, not parked forever.
    assert_eq!(
        queue.row("hostile").next_run_at,
        format_iso_ms(T0 + RETRY_MAX_MS)
    );

    assert_eq!(queue.tick(T0 + SCHEDULER_TICK_MS, ok), ["flaky"]);
    assert!(!queue.is_due("hostile", T0 + RETRY_MAX_MS - 1));
    assert_eq!(queue.tick(T0 + RETRY_MAX_MS, ok), ["hostile"]);
}

#[test]
fn permanent_failure_advances_or_disables_like_a_run() {
    let mut queue = Queue::default();
    queue.put("gone-once", T0 - 2, None);
    queue.put("gone-hourly", T0 - 1, Some(3_600));
    queue.put("next", T0, None);

    let posted = queue.tick(T0, |id| match id {
        "next" => Post::Ok,
        _ => Post::Failed {
            status: Some(404),
            retry_after_ms: None,
        },
    });
    assert_eq!(posted, ["next"]);
    assert!(!queue.row("gone-once").enabled);
    assert_eq!(
        queue.row("gone-hourly").next_run_at,
        format_iso_ms(T0 + HOUR_MS)
    );
    assert!(queue.tick(T0 + SCHEDULER_TICK_MS, ok).is_empty());
}

// ---- (5) resolve_scheduled_id refuses missing and ambiguous ids ----

#[test]
fn prefix_resolution_refuses_missing_and_ambiguous_ids() {
    let ids = ["a1b2c3d4", "a1b2ffff", "c0ffee00", "c0ffee001", "d00d"];
    let reversed: Vec<&str> = ids.iter().rev().copied().collect();
    let resolve = |prefix: &str| {
        let verdict = resolve_scheduled_id(ids.iter().copied(), prefix);
        assert_eq!(
            verdict,
            resolve_scheduled_id(reversed.iter().copied(), prefix),
            "input order must not decide {prefix:?}"
        );
        verdict
    };

    assert_eq!(resolve("a1b2c"), IdResolution::Unique("a1b2c3d4"));
    assert_eq!(resolve("c0ffee001"), IdResolution::Unique("c0ffee001"));
    assert_eq!(resolve("d00d"), IdResolution::Unique("d00d"));

    assert_eq!(resolve("zz"), IdResolution::Missing);
    assert_eq!(resolve("A1B2C3"), IdResolution::Missing, "case-sensitive");
    assert_eq!(
        resolve("d00d0"),
        IdResolution::Missing,
        "longer than any id"
    );
    // LIKE metacharacters are literal (the store uses `starts_with`).
    assert_eq!(resolve("a1b2%"), IdResolution::Missing);
    assert_eq!(resolve("a1b2_3d4"), IdResolution::Missing);
    assert_eq!(
        resolve_scheduled_id(std::iter::empty::<&str>(), "a1b2c3d4"),
        IdResolution::Missing
    );

    assert_eq!(resolve("a1b2"), IdResolution::Ambiguous);
    assert_eq!(
        resolve("c0ffee00"),
        IdResolution::Ambiguous,
        "an exact id with a longer sibling still refuses (legacy)"
    );
    assert_eq!(
        resolve(""),
        IdResolution::Ambiguous,
        "an empty prefix never picks an arbitrary row"
    );
    assert_eq!(
        resolve_scheduled_id(["only"], ""),
        IdResolution::Unique("only")
    );

    // Both refusals share one reply naming the input and the way out.
    for refused in ["zz", "a1b2"] {
        let reply = no_unique_match_text(refused);
        assert!(reply.contains(&format!("`{refused}`")), "{reply}");
        assert!(reply.contains("Use the full id from /schedule-list."));
    }
}
