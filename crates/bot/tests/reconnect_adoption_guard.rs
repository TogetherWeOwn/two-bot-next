//! Twilight already backs off before emitting Reconnect; it owns retries.
//! The receive arm adds only an awaited, non-growing decorrelation spread.
//! Pair this fail-closed wiring check with reconnect_spread_runtime_acceptance:
//! text alone cannot prove that the helper actually waits.
//!
//! Builder: use the fully qualified helper call as a direct arm statement
//! (optionally `let delay = ... .await;`), before the terminal `continue;`.
//! Comments/literals, an unpolled future, a conditional or an early exit must
//! not satisfy this guard. The separate supervised runner keeps its schedule.

const GATEWAY_SRC: &str = include_str!("../src/gateway.rs");

// Lex only what this narrow structural check needs. As in
// timing_sleep_removal.rs, comments (including nested blocks), raw strings,
// escaped strings and character literals cannot supply executable tokens.
fn skip_non_code(bytes: &[u8], at: usize) -> Option<usize> {
    if bytes.get(at..at + 2) == Some(b"//") {
        return Some(
            bytes[at..]
                .iter()
                .position(|b| *b == b'\n')
                .map_or(bytes.len(), |offset| at + offset),
        );
    }
    if bytes.get(at..at + 2) == Some(b"/*") {
        let mut cursor = at + 2;
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
    let raw = if bytes.get(at) == Some(&b'r') {
        Some(at)
    } else if bytes.get(at..at + 2) == Some(b"br") {
        Some(at + 1)
    } else {
        None
    };
    if let Some(raw) = raw {
        let mut quote = raw + 1;
        while bytes.get(quote) == Some(&b'#') {
            quote += 1;
        }
        if bytes.get(quote) == Some(&b'"') {
            let hashes = quote - raw - 1;
            let mut cursor = quote + 1;
            while cursor < bytes.len() {
                if bytes[cursor] == b'"'
                    && bytes.get(cursor + 1..cursor + 1 + hashes) == Some(&bytes[raw + 1..quote])
                {
                    return Some(cursor + 1 + hashes);
                }
                cursor += 1;
            }
            return Some(bytes.len());
        }
    }
    if matches!(bytes.get(at), Some(b'"' | b'\'')) {
        let quote = bytes[at];
        let mut cursor = at + 1;
        while cursor < bytes.len() {
            // A lifetime is not a character literal.
            if quote == b'\'' && cursor - at > 12 {
                return None;
            }
            match bytes[cursor] {
                b'\\' => cursor = (cursor + 2).min(bytes.len()),
                b if b == quote => return Some(cursor + 1),
                b'\n' if quote == b'\'' => return None,
                _ => cursor += 1,
            }
        }
        return (quote == b'"').then_some(bytes.len());
    }
    None
}

fn tokens(source: &str) -> Vec<&str> {
    let bytes = source.as_bytes();
    let mut at = 0;
    let mut result = Vec::new();
    while at < bytes.len() {
        if let Some(next) = skip_non_code(bytes, at) {
            at = next;
        } else if bytes[at].is_ascii_whitespace() {
            at += 1;
        } else {
            let start = at;
            at += 1;
            if bytes[start].is_ascii_alphanumeric() || bytes[start] == b'_' {
                while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
                    at += 1;
                }
            } else {
                while !source.is_char_boundary(at) {
                    at += 1;
                }
            }
            result.push(&source[start..at]);
        }
    }
    result
}

fn awaited_spread_is_adopted(source: &str) -> bool {
    let code = tokens(source);
    let marker = ["ReceiveMessageErrorType", ":", ":", "Reconnect"];
    let arms: Vec<_> = code
        .windows(marker.len())
        .enumerate()
        .filter_map(|(at, window)| (window == marker).then_some(at))
        .collect();
    if arms.len() != 1 {
        return false;
    }
    let tail = &code[arms[0] + marker.len()..];
    let Some(open) = tail.windows(3).position(|w| w == ["=", ">", "{"]) else {
        return false;
    };
    let mut depth = 1;
    let mut end = None;
    for (at, token) in tail.iter().enumerate().skip(open + 3) {
        match *token {
            "{" => depth += 1,
            "}" => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            end = Some(at);
            break;
        }
    }
    let Some(end) = end else {
        return false;
    };
    let body = &tail[open + 3..end];
    // No stacked exponential wait, additional sleep, bypass or conditional
    // adoption. Any deliberate arm redesign needs a new protected test.
    if body.iter().any(|t| {
        matches!(
            *t,
            "sleep"
                | "sleep_until"
                | "reconnect_delay"
                | "run_supervised"
                | "return"
                | "break"
                | "if"
                | "match"
                | "loop"
                | "for"
                | "while"
        )
    }) {
        return false;
    }
    let mut nesting = 0i32;
    let mut start = 0;
    let mut statements = Vec::new();
    for (at, token) in body.iter().enumerate() {
        match *token {
            "(" | "[" | "{" => nesting += 1,
            ")" | "]" | "}" => nesting -= 1,
            ";" if nesting == 0 => {
                statements.push(&body[start..at]);
                start = at + 1;
            }
            _ => {}
        }
        if nesting < 0 {
            return false;
        }
    }
    if nesting != 0 || start != body.len() || statements.last() != Some(&&["continue"][..]) {
        return false;
    }
    if body.iter().filter(|t| **t == "continue").count() != 1
        || body
            .iter()
            .filter(|t| **t == "wait_reconnect_spread")
            .count()
            != 1
    {
        return false;
    }
    let helper = [
        "two_bot",
        ":",
        ":",
        "gateway_backoff",
        ":",
        ":",
        "wait_reconnect_spread",
        "(",
    ];
    statements
        .iter()
        .take(statements.len() - 1)
        .any(|statement| {
            let expression = if statement.first() == Some(&"let") {
                if statement.get(2) != Some(&"=") {
                    return false;
                }
                &statement[3..]
            } else {
                statement
            };
            if !expression.starts_with(&helper) {
                return false;
            }
            let mut parens = 1;
            for (at, token) in expression.iter().enumerate().skip(helper.len()) {
                match *token {
                    "(" => parens += 1,
                    ")" => parens -= 1,
                    _ => {}
                }
                if parens == 0 {
                    return expression[at + 1..] == [".", "await"];
                }
            }
            false
        })
}

#[test]
fn reconnect_arm_awaits_the_non_growing_spread_before_continue() {
    assert!(
        awaited_spread_is_adopted(GATEWAY_SRC),
        "Reconnect arm must directly await two_bot::gateway_backoff::wait_reconnect_spread(seed, attempt) before continue, with no stacked schedule; Twilight already owns reconnect backoff"
    );
}

fn arm(body: &str) -> String {
    format!(
        "match item {{ Err(error) if matches!(error.kind(), twilight_gateway::error::ReceiveMessageErrorType::Reconnect) => {{ {body} }} Err(_) => return, }}"
    )
}

const CALL: &str = "two_bot::gateway_backoff::wait_reconnect_spread(seed, attempt)";

#[test]
fn structural_guard_accepts_direct_await_and_formatting() {
    assert!(awaited_spread_is_adopted(&arm(&format!(
        "let delay = {CALL}.await; warn!(wait_ms = delay.as_millis()); continue;"
    ))));
    assert!(awaited_spread_is_adopted(&arm(
        "two_bot :: gateway_backoff :: wait_reconnect_spread(\n seed, /* spread */ attempt,\n).await; continue;"
    )));
}

#[test]
fn structural_guard_rejects_sleep_deletion_bypass_and_stacked_schedule() {
    for body in [
        "// sleep reconnect_delay gateway_backoff\ncontinue;".to_owned(),
        format!("let _hint = r###\"{CALL}.await;\"###; continue;"),
        format!("let _future = {CALL}; continue;"),
        format!("let _delay = {CALL}.now_or_never().unwrap_or_default() + other_wait().await; continue;"),
        format!("if false {{ {CALL}.await; }} continue;"),
        format!("continue; {CALL}.await;"),
        format!("let _delay = async {{ {CALL}.await }}; continue;"),
        format!("let delay = {CALL}.await; tokio::time::sleep(delay).await; continue;"),
        "let delay = reconnect_delay(&policy, attempt).unwrap_or(policy.max_delay); tokio::time::sleep(delay).await; continue;".to_owned(),
        "continue;".to_owned(),
    ] {
        assert!(!awaited_spread_is_adopted(&arm(&body)), "guard admitted: {body}");
    }
}
