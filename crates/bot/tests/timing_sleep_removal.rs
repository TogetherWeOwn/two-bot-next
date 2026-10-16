//! Bounded-backoff acceptance (roadmap M2.4).
//!
//! Paired acceptance for `tests: replace timing sleeps with bounded polling`:
//! every guard below FAILS while its target busy-spins on bare scheduler
//! yields or waits on an unbounded fixed delay, and passes once the site
//! polls with a bounded millisecond backoff under its deadline. A guard goes
//! red again if a long sleep returns or the backoff is dropped, so the sites
//! stay pinned without retries and without ignored tests (house flake-ledger
//! policy forbids both).
//!
//! Targets, all test files only (no production code-path changes):
//!
//! 1. `alive.rs`: three 10 ms polls (`wait_http`, `checkpoint`, first-boot
//!    events tick) each under the STEP deadline, plus a 200 ms post-ready
//!    settle window that polls liveness on a 10 ms backoff before SIGTERM.
//! 2. `ops_backfill.rs`: 20 ms blocking backoff in the `spawn_bot` health
//!    poll under its 15 s deadline.
//! 3. `preflight.rs`: the 429 / `retry_after` lane-hold tests, sleep-free;
//!    the guard below locks that property plus the exact
//!    one-attempt-no-retry pins.
//!
//! Builder contract (renegotiate on the card, never by editing this file to
//! loosen it): keep every poll backoff a literal 1–50 ms duration under a
//! deadline, keep the post-ready settle window, and keep every other
//! assertion in those files byte-identical. This file itself never sleeps.

const ALIVE: &str = include_str!("alive.rs");
const OPS_BACKFILL: &str = include_str!("ops_backfill.rs");
const PREFLIGHT: &str = include_str!("preflight.rs");

fn skip_non_code(bytes: &[u8], index: usize) -> Option<usize> {
    if bytes.get(index..index + 2) == Some(b"//") {
        return Some(
            bytes[index + 2..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |offset| index + 2 + offset),
        );
    }
    if bytes.get(index..index + 2) == Some(b"/*") {
        let mut cursor = index + 2;
        let mut depth = 1;
        while cursor + 1 < bytes.len() {
            match &bytes[cursor..cursor + 2] {
                b"/*" => {
                    depth += 1;
                    cursor += 2;
                }
                b"*/" => {
                    depth -= 1;
                    cursor += 2;
                    if depth == 0 {
                        return Some(cursor);
                    }
                }
                _ => cursor += 1,
            }
        }
        return Some(bytes.len());
    }

    let raw_start = if bytes.get(index) == Some(&b'r') {
        Some(index)
    } else if bytes.get(index..index + 2) == Some(b"br") {
        Some(index + 1)
    } else {
        None
    };
    if let Some(raw_start) = raw_start {
        let mut quote = raw_start + 1;
        while bytes.get(quote) == Some(&b'#') {
            quote += 1;
        }
        if bytes.get(quote) == Some(&b'"') {
            let hashes = quote - raw_start - 1;
            let mut cursor = quote + 1;
            while cursor < bytes.len() {
                if bytes[cursor] == b'"'
                    && bytes.get(cursor + 1..cursor + 1 + hashes)
                        == Some(&bytes[raw_start + 1..quote])
                {
                    return Some(cursor + 1 + hashes);
                }
                cursor += 1;
            }
            return Some(bytes.len());
        }
    }

    if matches!(bytes.get(index), Some(b'"' | b'\'')) {
        let quote_index = index;
        let quote = bytes[quote_index];
        let mut cursor = quote_index + 1;
        while cursor < bytes.len() {
            if quote == b'\'' && cursor - quote_index > 12 {
                return None;
            }
            match bytes[cursor] {
                b'\\' => cursor = (cursor + 2).min(bytes.len()),
                byte if byte == quote => return Some(cursor + 1),
                b'\n' if quote == b'\'' => return None,
                _ => cursor += 1,
            }
        }
        return (quote == b'"').then_some(bytes.len());
    }
    None
}

fn code_only(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut code = bytes.to_vec();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code(bytes, cursor) {
            for index in cursor..next {
                if !matches!(bytes[index], b'\n' | b'\r') {
                    code[index] = b' ';
                }
            }
            cursor = next;
        } else {
            cursor += 1;
        }
    }
    String::from_utf8(code).expect("masking source preserves UTF-8")
}

fn braced_block(source: &str, open: usize) -> String {
    let bytes = source.as_bytes();
    let mut cursor = open;
    let mut depth = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code(bytes, cursor) {
            cursor = next;
            continue;
        }
        match bytes[cursor] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return source[open..=cursor].to_owned();
                }
            }
            _ => {}
        }
        cursor += 1;
    }
    panic!("unterminated block at byte {open}");
}

fn function_body(source: &str, name: &str) -> String {
    let signature = source
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("missing function {name}"));
    let body_start = source[signature..]
        .find('{')
        .map(|offset| signature + offset)
        .unwrap_or_else(|| panic!("missing body for function {name}"));
    braced_block(source, body_start)
}

fn loop_body(source: &str, marker: &str) -> String {
    let source = code_only(source);
    let marker_start = source
        .find(marker)
        .unwrap_or_else(|| panic!("missing poll loop {marker:?}"));
    let open = source[marker_start..]
        .find('{')
        .map(|offset| marker_start + offset)
        .unwrap_or_else(|| panic!("missing poll loop body {marker:?}"));
    braced_block(&source, open)
}

fn source_between(source: &str, start: &str, end: &str) -> String {
    let start = source
        .find(start)
        .unwrap_or_else(|| panic!("missing source boundary {start:?}"));
    let end = source[start..]
        .find(end)
        .map(|offset| start + offset)
        .unwrap_or_else(|| panic!("missing source boundary {end:?}"));
    source[start..end].to_owned()
}

fn parse_sleep_millis(sleep_args: &str, context: &str) -> u64 {
    let (millis, suffix) = sleep_args
        .strip_prefix("Duration::from_millis(")
        .and_then(|args| args.split_once(')'))
        .unwrap_or_else(|| panic!("{context} must use a literal millisecond duration"));
    assert!(
        suffix.starts_with(").await;") || suffix.starts_with(");"),
        "{context} must pass only its literal duration to sleep"
    );
    assert!(
        !millis.is_empty() && millis.bytes().all(|byte| byte.is_ascii_digit()),
        "{context} must use a literal millisecond duration"
    );
    let millis = millis
        .parse::<u64>()
        .unwrap_or_else(|_| panic!("{context} must use a literal millisecond duration"));
    assert!(
        (1..=50).contains(&millis),
        "{context} must be between 1 and 50 ms"
    );
    millis
}

fn assert_code_contains(source: &str, snippet: &str, context: &str) {
    // Mask both sides: a snippet hidden in a comment or literal in the
    // source must not satisfy the pin, and string literals inside the
    // snippet itself are masked to the same spaces on both sides.
    assert!(code_only(source).contains(&code_only(snippet)), "{context}");
}

fn assert_no_code_contains(source: &str, snippet: &str, context: &str) {
    assert!(
        !code_only(source).contains(&code_only(snippet)),
        "{context}"
    );
}

fn contains_continue_keyword(code: &str) -> bool {
    let bytes = code.as_bytes();
    let needle = b"continue";
    let mut index = 0;
    while index + needle.len() <= bytes.len() {
        if &bytes[index..index + needle.len()] == needle {
            let before_ok = index == 0
                || !(bytes[index - 1].is_ascii_alphanumeric() || bytes[index - 1] == b'_');
            let after = index + needle.len();
            let after_ok = after >= bytes.len()
                || !(bytes[after].is_ascii_alphanumeric() || bytes[after] == b'_');
            if before_ok && after_ok {
                return true;
            }
            index += needle.len();
        } else {
            index += 1;
        }
    }
    false
}

fn assert_poll_site(source: &str, loop_marker: &str) {
    let body = loop_body(source, loop_marker);
    assert!(
        !contains_continue_keyword(&body),
        "poll loop must not bypass its backoff with continue"
    );
    assert_eq!(
        body.matches("sleep(").count(),
        1,
        "each poll loop must contain exactly one sleep call"
    );
    let (before_sleep, sleep_args) = body.split_once("sleep(").expect("sleep call is present");
    // The sleep must be a direct statement of the loop body (brace depth 1,
    // just inside the loop's own braces), not nested in a conditional or
    // block that may never run while the loop busy-spins.
    let mut depth = 0i32;
    for byte in before_sleep.bytes() {
        match byte {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
    }
    assert_eq!(
        depth, 1,
        "poll backoff must be a direct statement of the poll loop body"
    );
    parse_sleep_millis(sleep_args, "poll backoff");
}

/// Require `snippet` to appear as a direct, reachable statement of
/// `loop_body`: brace depth 1 just inside the loop's own braces, matched over
/// code only so a commented-out or `if false`-wrapped copy does not satisfy
/// the pin.
fn assert_direct_statement_at_depth(source: &str, snippet: &str, depth: i32, context: &str) {
    let code = code_only(source);
    let masked_snippet = code_only(snippet);
    let pos = code.find(&masked_snippet).unwrap_or_else(|| {
        panic!("{context}");
    });
    assert_eq!(brace_depth_before(&code, pos), depth, "{context}");
}

fn assert_direct_loop_statement(loop_body: &str, snippet: &str, context: &str) {
    assert_direct_statement_at_depth(loop_body, snippet, 1, context);
}

fn assert_sleep_inventory(source: &str, expected: usize, context: &str) {
    // Whole-file inventory over code only: a stray sleep anywhere in the
    // file (not just inside the named poll loops) must go red, whether it
    // changes the count or uses a non-literal / non-millisecond duration.
    let code = code_only(source);
    let sleeps: Vec<&str> = code.split("sleep(").collect();
    assert_eq!(
        sleeps.len() - 1,
        expected,
        "{context} must contain exactly {expected} sleep call(s)"
    );
    for sleep_args in &sleeps[1..] {
        parse_sleep_millis(sleep_args, context);
    }
}

const OPS_DEADLINE_ASSERT: &str = r#"assert!(
            Instant::now() < deadline,
            "bot did not serve health on the configured port"
        );"#;

fn brace_depth_before(code: &str, pos: usize) -> i32 {
    let mut depth = 0i32;
    for byte in code[..pos].bytes() {
        match byte {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
    }
    depth
}

fn timeout_step_async_body(source: &str, context: &str) -> String {
    // The first `{` after `timeout(STEP` opens the async block; the poll
    // loop must live inside those braces, not merely between the timeout
    // call and a later terminator. Matched over code only so a
    // commented-out wrapper does not satisfy the pin.
    let code = code_only(source);
    let start = code
        .find("timeout(STEP")
        .unwrap_or_else(|| panic!("{context}"));
    let open = code[start..]
        .find('{')
        .map(|offset| start + offset)
        .unwrap_or_else(|| panic!("{context}"));
    braced_block(&code, open)
}

fn assert_loop_inside_step_timeout(source: &str, context: &str) {
    let body = timeout_step_async_body(source, context);
    assert_poll_site(&body, "loop");
}

fn assert_settle_ungated(lifecycle: &str) {
    // The settle window must run every boot at the same nesting as SIGTERM.
    // Wrapping it in `if boot == 0` drops the post-RESUMED window on boot 1
    // (the exact gap the window was added to close); wrapping it in
    // `if false` drops it entirely. Both keep the settle text present, so
    // the depth of `let settle_until` must equal the depth of `terminate`.
    let code = code_only(lifecycle);
    let settle_pos = code
        .find("let settle_until =")
        .expect("settle window must declare its deadline");
    let term_pos = code
        .find("bot.terminate().await;")
        .expect("lifecycle must terminate the bot");
    assert_eq!(
        brace_depth_before(&code, settle_pos),
        brace_depth_before(&code, term_pos),
        "settle window must sit at the same depth as SIGTERM so it runs every boot"
    );
}

fn assert_ops_backfill_deadline(source: &str) {
    assert_code_contains(
        source,
        OPS_DEADLINE_ASSERT,
        "spawn_bot health poll must retain its exact deadline assertion",
    );
    assert_code_contains(
        source,
        "let deadline = Instant::now() + Duration::from_secs(15);",
        "spawn_bot must retain its 15 s deadline",
    );
    // Placement: the deadline assertion must be a direct statement of the
    // health-poll loop body. A copy moved above the loop or wrapped in
    // `if false` leaves the text present but the poll hangs past its
    // deadline instead of failing at it.
    let body = loop_body(source, "loop");
    assert_direct_loop_statement(
        &body,
        OPS_DEADLINE_ASSERT,
        "spawn_bot deadline assertion must be a direct statement inside the health-poll loop body",
    );
}

/// Require the poll loop to sit inside its `timeout(STEP, ...)` async
/// block: the loop must be found inside the braced block opened by the
/// timeout, matched over code only so a commented-out wrapper does not
/// satisfy the pin. A loop hoisted after the timeout block but before the
/// terminator still sits between the two markers textually, so the
/// between-markers check alone is placement-blind.
fn assert_wait_http_poll_under_step_deadline(wait_http: &str) {
    assert_loop_inside_step_timeout(wait_http, "wait_http must poll inside its STEP deadline");
}

fn assert_budget_lane_hold_pin(source: &str) {
    let code = code_only(source);
    let loop_start = code
        .find("for (name, gateway_response) in [")
        .expect("budget test must retain its response loop");
    let loop_open = code[loop_start..]
        .find("] {")
        .map(|offset| loop_start + offset + 2)
        .expect("budget response loop must retain its opening brace");
    let response_loop = braced_block(&code, loop_open);
    assert_direct_loop_statement(
        &response_loop,
        r#"assert_eq!(mock.requests().len(), 3, "{name}: lane held, no retry");"#,
        "budget lane-hold assertion must be a direct statement inside the response loop",
    );
}

fn assert_preflight_429_attempt_pin(source: &str) {
    let code = code_only(source);
    assert!(code.starts_with('{') && code.ends_with('}'));
    assert_direct_statement_at_depth(
        &code[1..code.len() - 1],
        "assert_eq!(mock.requests().len(), 1);",
        0,
        "preflight 429 one-attempt assertion must be a direct top-level test statement",
    );
}

fn assert_settle_window(source: &str) {
    let source = code_only(source);
    assert!(
        source.contains(
            "let settle_until = tokio::time::Instant::now() + Duration::from_millis(200);"
        ),
        "post-ready settle deadline must start 200 ms in the future"
    );
    assert!(
        source.contains("while tokio::time::Instant::now() < settle_until {"),
        "post-ready settle loop must retain its ungated deadline header"
    );
    let settle_loop = loop_body(&source, "while tokio::time::Instant::now() < settle_until");
    assert_eq!(
        settle_loop.matches("bot.assert_alive();").count(),
        1,
        "settle loop must assert liveness during the window"
    );
    // Placement: the in-window liveness check must be a direct, reachable
    // statement of the settle loop body. An `if false`-wrapped copy keeps
    // the text present while a post-RESUMED crash goes undetected.
    assert_direct_loop_statement(
        &settle_loop,
        "bot.assert_alive();",
        "settle-loop liveness check must be a direct reachable statement of the settle loop body",
    );
    assert_eq!(
        source.matches("bot.assert_alive();").count(),
        2,
        "settle loop must assert liveness inside and after the window"
    );
    assert_poll_site(&source, "while tokio::time::Instant::now() < settle_until");
}

#[test]
fn alive_polling_uses_bounded_backoff_under_deadline() {
    let wait_http = function_body(ALIVE, "wait_http");
    assert_wait_http_poll_under_step_deadline(&wait_http);

    let checkpoint = function_body(ALIVE, "checkpoint");
    assert_loop_inside_step_timeout(&checkpoint, "checkpoint must poll inside its STEP deadline");

    let lifecycle = function_body(ALIVE, "lifecycle");
    let events_tick = source_between(&lifecycle, "let ticked = timeout(STEP", "ticked.is_ok()");
    assert_loop_inside_step_timeout(
        &events_tick,
        "events tick must poll inside its STEP deadline",
    );
    let settle = source_between(&lifecycle, "let settle_until =", "assert_ready_correlation");
    assert_settle_window(&settle);
    assert_settle_ungated(&lifecycle);

    assert_sleep_inventory(ALIVE, 4, "alive.rs sleep inventory");
    assert_no_code_contains(
        ALIVE,
        "yield_now",
        "alive.rs must not busy-spin on bare scheduler yields",
    );
}

#[test]
fn ops_backfill_spawn_uses_bounded_backoff_under_deadline() {
    let spawn_bot = function_body(OPS_BACKFILL, "spawn_bot");
    assert_poll_site(&spawn_bot, "loop");
    assert_ops_backfill_deadline(&spawn_bot);
    assert_sleep_inventory(OPS_BACKFILL, 1, "ops_backfill.rs sleep inventory");
    assert_no_code_contains(
        &spawn_bot,
        "yield_now",
        "ops_backfill.rs must not busy-spin on bare scheduler yields",
    );
}

#[test]
fn preflight_429_lane_hold_stays_sleep_free_with_one_attempt_pins() {
    let budget_test = function_body(
        PREFLIGHT,
        "session_start_budget_pass_warn_fail_and_fail_closed",
    );
    assert_no_code_contains(
        &budget_test,
        "sleep(",
        "budget lane-hold tests must remain free of timing sleeps",
    );
    assert_budget_lane_hold_pin(&budget_test);

    let preflight_test = function_body(
        PREFLIGHT,
        "preflight_429_is_one_attempt_and_stops_the_check_sequence",
    );
    assert_no_code_contains(
        &preflight_test,
        "sleep(",
        "preflight 429 test must remain free of timing sleeps",
    );
    assert_preflight_429_attempt_pin(&preflight_test);
    assert_sleep_inventory(PREFLIGHT, 0, "preflight.rs sleep inventory");
}

#[test]
fn source_block_parser_ignores_braces_in_comments_and_literals() {
    let source = r####"fn sample() {
        let close = "}";
        let raw = r#"{ still a string }"#;
        /* { nested /* } */ comment } */
        loop { break; }
    }"####;
    let open = source.find('{').unwrap();
    let block = braced_block(source, open);
    assert!(block.contains("loop { break; }"));
    assert!(block.ends_with("    }"));
}

#[test]
fn polling_guard_rejects_missing_or_unbounded_site_backoffs() {
    let wait_http = function_body(ALIVE, "wait_http");
    assert_poll_site(&wait_http, "loop");
    assert!(std::panic::catch_unwind(|| {
        assert_poll_site(
            &wait_http.replace("sleep(Duration::from_millis(10)).await;", ""),
            "loop",
        );
    })
    .is_err());
    assert!(std::panic::catch_unwind(|| {
        assert_poll_site(
            &wait_http.replace(
                "sleep(Duration::from_millis(10)).await;",
                "sleep(Duration::from_millis(backoff_ms)).await;",
            ),
            "loop",
        );
    })
    .is_err());
    assert!(std::panic::catch_unwind(|| {
        assert_poll_site(
            &wait_http.replace(
                "sleep(Duration::from_millis(10)).await;",
                "sleep(Duration::from_secs(1)).await;",
            ),
            "loop",
        );
    })
    .is_err());

    let lifecycle = function_body(ALIVE, "lifecycle");
    let settle = source_between(&lifecycle, "let settle_until =", "assert_ready_correlation");
    assert_settle_window(&settle);
    let flipped_deadline = settle.replace(
        "Instant::now() + Duration::from_millis(200)",
        "Instant::now() - Duration::from_millis(200)",
    );
    assert!(std::panic::catch_unwind(|| assert_settle_window(&flipped_deadline)).is_err());
    assert!(std::panic::catch_unwind(|| {
        assert_settle_window(
            &settle.replace("Duration::from_millis(200)", "Duration::from_millis(0)"),
        );
    })
    .is_err());
    assert!(std::panic::catch_unwind(|| {
        assert_settle_window(
            &settle.replace("bot.assert_alive();", "// liveness assertion removed"),
        );
    })
    .is_err());

    let sleep = "sleep(Duration::from_millis(10)).await;";
    let moved_wait_http =
        wait_http
            .replace(sleep, "")
            .replacen("loop {", &format!("{sleep}\n        loop {{"), 1);
    assert!(std::panic::catch_unwind(|| {
        assert_poll_site(&moved_wait_http, "loop");
    })
    .is_err());

    let moved_settle_sleep = format!("{}\n{sleep}", settle.replace(sleep, ""));
    assert!(std::panic::catch_unwind(|| {
        assert_settle_window(&moved_settle_sleep);
    })
    .is_err());

    let commented_wait_http = wait_http.replace(sleep, &format!("// {sleep}"));
    assert!(std::panic::catch_unwind(|| {
        assert_poll_site(&commented_wait_http, "loop");
    })
    .is_err());

    let commented_settle_duration = settle.replace(
        "Duration::from_millis(200)",
        "// Duration::from_millis(200)",
    );
    assert!(std::panic::catch_unwind(|| {
        assert_settle_window(&commented_settle_duration);
    })
    .is_err());

    // A sleep nested in a never-taken branch is not a loop backoff.
    let if_false_wait_http = wait_http.replace(sleep, &format!("if false {{ {sleep} }}"));
    assert!(std::panic::catch_unwind(|| {
        assert_poll_site(&if_false_wait_http, "loop");
    })
    .is_err());

    // A `continue` before the backoff bypasses it every iteration (M10: the
    // checkpoint loop busy-spins on store loads when `seq` never equals its
    // target) while the sleep text remains, so the guard must reject any
    // `continue` inside a poll loop body.
    let continue_wait_http = wait_http.replace(
        sleep,
        "if true { continue; }\n            sleep(Duration::from_millis(10)).await;",
    );
    assert!(std::panic::catch_unwind(|| {
        assert_poll_site(&continue_wait_http, "loop");
    })
    .is_err());

    // A stray long sleep outside every named poll loop breaks the
    // whole-file inventory even though each named site still passes.
    assert_sleep_inventory(ALIVE, 4, "alive.rs sleep inventory");
    let stray_long_sleep = ALIVE.replacen(
        "bot.assert_alive();",
        "sleep(Duration::from_secs(5)).await;\n            bot.assert_alive();",
        1,
    );
    assert!(std::panic::catch_unwind(|| {
        assert_sleep_inventory(&stray_long_sleep, 4, "alive.rs sleep inventory");
    })
    .is_err());
}

#[test]
fn polling_guard_pins_exact_ops_deadline_and_preflight_attempt_assertions() {
    let spawn_bot = function_body(OPS_BACKFILL, "spawn_bot");
    let without_deadline_check = spawn_bot.replace(
        "assert!(\n            Instant::now() < deadline,\n            \"bot did not serve health on the configured port\"\n        );",
        "// deadline check removed",
    );
    assert!(without_deadline_check.contains("deadline"));
    assert!(std::panic::catch_unwind(|| {
        assert_ops_backfill_deadline(&without_deadline_check);
    })
    .is_err());

    let preflight_test = function_body(
        PREFLIGHT,
        "preflight_429_is_one_attempt_and_stops_the_check_sequence",
    );
    let without_attempt_pin = preflight_test.replace("assert_eq!(mock.requests().len(), 1);", "");
    assert!(std::panic::catch_unwind(|| {
        assert_preflight_429_attempt_pin(&without_attempt_pin);
    })
    .is_err());

    // Block-commenting a pinned assertion must not satisfy its pin: the
    // raw text is still present, but it is no longer code.
    let deadline_assert = "assert!(\n            Instant::now() < deadline,\n            \"bot did not serve health on the configured port\"\n        );";
    let commented_deadline =
        spawn_bot.replace(deadline_assert, &format!("/* {deadline_assert} */"));
    assert!(commented_deadline.contains(deadline_assert));
    assert!(std::panic::catch_unwind(|| {
        assert_ops_backfill_deadline(&commented_deadline);
    })
    .is_err());

    let commented_attempt_pin = preflight_test.replace(
        "assert_eq!(mock.requests().len(), 1);",
        "/* assert_eq!(mock.requests().len(), 1); */",
    );
    assert!(std::panic::catch_unwind(|| {
        assert_preflight_429_attempt_pin(&commented_attempt_pin);
    })
    .is_err());

    let budget_test = function_body(
        PREFLIGHT,
        "session_start_budget_pass_warn_fail_and_fail_closed",
    );
    let budget_pin = r#"assert_eq!(mock.requests().len(), 3, "{name}: lane held, no retry");"#;
    let commented_budget_pin = budget_test.replace(budget_pin, &format!("/* {budget_pin} */"));
    assert!(std::panic::catch_unwind(|| {
        assert_budget_lane_hold_pin(&commented_budget_pin);
    })
    .is_err());
    let if_false_budget_pin =
        budget_test.replace(budget_pin, &format!("if false {{ {budget_pin} }}"));
    assert!(std::panic::catch_unwind(|| {
        assert_budget_lane_hold_pin(&if_false_budget_pin);
    })
    .is_err());

    let if_false_attempt_pin = preflight_test.replace(
        "assert_eq!(mock.requests().len(), 1);",
        "if false { assert_eq!(mock.requests().len(), 1); }",
    );
    assert!(std::panic::catch_unwind(|| {
        assert_preflight_429_attempt_pin(&if_false_attempt_pin);
    })
    .is_err());

    let wait_http = function_body(ALIVE, "wait_http");
    // The positive control passes through the placement-aware helper: the
    // loop sits inside its STEP wrapper in the real source.
    assert_wait_http_poll_under_step_deadline(&wait_http);
    let commented_deadline_call = wait_http.replace("timeout(STEP", "/* timeout(STEP */ timeout(");
    assert!(std::panic::catch_unwind(|| {
        assert_wait_http_poll_under_step_deadline(&commented_deadline_call);
    })
    .is_err());

    // The 15 s deadline literal must be pinned: stretching it to a day
    // keeps every other assertion present while the health poll hangs.
    let long_deadline = spawn_bot.replace("Duration::from_secs(15)", "Duration::from_secs(86400)");
    assert!(long_deadline.contains("86400"));
    assert!(std::panic::catch_unwind(|| {
        assert_ops_backfill_deadline(&long_deadline);
    })
    .is_err());

    // Positive controls: checkpoint and events-tick loops sit inside
    // their STEP wrappers in the real source.
    let checkpoint = function_body(ALIVE, "checkpoint");
    assert_loop_inside_step_timeout(&checkpoint, "checkpoint must poll inside its STEP deadline");
    let lifecycle = function_body(ALIVE, "lifecycle");
    let events_tick = source_between(&lifecycle, "let ticked = timeout(STEP", "ticked.is_ok()");
    assert_loop_inside_step_timeout(
        &events_tick,
        "events tick must poll inside its STEP deadline",
    );
}

#[test]
fn polling_guard_rejects_misplaced_deadline_and_liveness_checks() {
    // The ops_backfill deadline text stays present but moves above the poll
    // loop: the poll then hangs past its deadline instead of failing at it.
    let spawn_bot = function_body(OPS_BACKFILL, "spawn_bot");
    assert_ops_backfill_deadline(&spawn_bot);
    let moved_above = spawn_bot.replace(OPS_DEADLINE_ASSERT, "").replacen(
        "loop {",
        &format!("{OPS_DEADLINE_ASSERT}\n        loop {{"),
        1,
    );
    assert!(moved_above.contains("Instant::now() < deadline"));
    assert!(std::panic::catch_unwind(|| {
        assert_ops_backfill_deadline(&moved_above);
    })
    .is_err());

    // The deadline text stays present but is wrapped in a never-taken
    // branch: the loop body no longer executes it while health hangs.
    let if_false_deadline = spawn_bot.replace(
        OPS_DEADLINE_ASSERT,
        &format!("if false {{ {OPS_DEADLINE_ASSERT} }}"),
    );
    assert!(std::panic::catch_unwind(|| {
        assert_ops_backfill_deadline(&if_false_deadline);
    })
    .is_err());

    // The `timeout(STEP` wrapper text stays present but the poll loop is
    // hoisted outside it: the loop then runs without the STEP deadline.
    let wait_http = function_body(ALIVE, "wait_http");
    let sleep = "sleep(Duration::from_millis(10)).await;";
    let loop_outside_timeout = format!(
        "loop {{ {sleep} }}\n{}",
        wait_http.replacen("loop {", "if true {", 1)
    );
    assert!(loop_outside_timeout.contains("timeout(STEP"));
    assert!(std::panic::catch_unwind(|| {
        assert_wait_http_poll_under_step_deadline(&loop_outside_timeout);
    })
    .is_err());

    // The settle-loop liveness text stays present but is wrapped in a
    // never-taken branch: a post-RESUMED crash would go undetected.
    let lifecycle = function_body(ALIVE, "lifecycle");
    let settle = source_between(&lifecycle, "let settle_until =", "assert_ready_correlation");
    assert_settle_window(&settle);
    let boot_gated_settle = settle.replace(
        "while tokio::time::Instant::now() < settle_until {",
        "while tokio::time::Instant::now() < settle_until && boot == 0 {",
    );
    assert!(boot_gated_settle
        .contains("while tokio::time::Instant::now() < settle_until && boot == 0 {"));
    assert!(std::panic::catch_unwind(|| assert_settle_window(&boot_gated_settle)).is_err());

    let if_false_liveness =
        settle.replacen("bot.assert_alive();", "if false { bot.assert_alive(); }", 1);
    assert_eq!(
        code_only(&if_false_liveness)
            .matches("bot.assert_alive();")
            .count(),
        2,
        "mutant must keep both liveness texts present"
    );
    assert!(std::panic::catch_unwind(|| {
        assert_settle_window(&if_false_liveness);
    })
    .is_err());

    // The checkpoint loop text stays present but is hoisted outside its
    // STEP wrapper: it then runs unbounded until the CI step timeout.
    let checkpoint = function_body(ALIVE, "checkpoint");
    let loop_outside_checkpoint = format!(
        "loop {{ {sleep} }}\n{}",
        checkpoint.replacen("loop {", "if true {", 1)
    );
    assert!(loop_outside_checkpoint.contains("timeout(STEP"));
    assert!(std::panic::catch_unwind(|| {
        assert_loop_inside_step_timeout(
            &loop_outside_checkpoint,
            "checkpoint must poll inside its STEP deadline",
        );
    })
    .is_err());

    // The events-tick loop stays between the ticked markers but moves out
    // of the timeout async block: the window text still contains a loop,
    // so only the inside-the-braces check goes red.
    let events_tick = source_between(&lifecycle, "let ticked = timeout(STEP", "ticked.is_ok()");
    let loop_outside_tick = format!(
        "loop {{ {sleep} }}\n{}",
        events_tick.replacen("loop {", "if true {", 1)
    );
    assert!(loop_outside_tick.contains("timeout(STEP"));
    assert!(std::panic::catch_unwind(|| {
        assert_loop_inside_step_timeout(
            &loop_outside_tick,
            "events tick must poll inside its STEP deadline",
        );
    })
    .is_err());

    // The settle window text stays present but is gated to boot 0 or to a
    // never-taken branch: boot 1 then skips the post-RESUMED window, the
    // exact gap the window was added to close.
    assert_settle_ungated(&lifecycle);
    let gated_boot_zero = lifecycle
        .replacen("let settle_until =", "if boot == 0 { let settle_until =", 1)
        .replacen("bot.terminate().await;", "} bot.terminate().await;", 1);
    assert!(gated_boot_zero.contains("let settle_until ="));
    assert!(std::panic::catch_unwind(|| {
        assert_settle_ungated(&gated_boot_zero);
    })
    .is_err());
    let gated_dead = lifecycle
        .replacen("let settle_until =", "if false { let settle_until =", 1)
        .replacen("bot.terminate().await;", "} bot.terminate().await;", 1);
    assert!(std::panic::catch_unwind(|| {
        assert_settle_ungated(&gated_dead);
    })
    .is_err());
}
