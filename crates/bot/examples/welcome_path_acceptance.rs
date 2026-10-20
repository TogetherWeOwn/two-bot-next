// Test-only executable: compile the real runtime without its DB-backed cfg(test)
// modules. Run via tests/onboarding/test_welcome_path_acceptance.py, not cargo run.
#[cfg(not(test))]
extern crate self as sqlx;
#[cfg(not(test))]
extern crate self as two_bot_core;
#[cfg(not(test))]
extern crate self as two_bot_cutover;
#[cfg(not(test))]
extern crate self as two_bot_discord;

#[cfg(not(test))]
include!("../tests/welcome_path_acceptance/suite.rs");

#[cfg(test)]
fn main() {}
