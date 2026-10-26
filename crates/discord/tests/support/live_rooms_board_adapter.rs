//! Editable test-only bridge for the future pure board implementation.
//!
//! Baseline has no board API. This explicit missing-feature result allows the
//! protected fixtures to compile and fail assertions, not fail to import a
//! nonexistent module. Builder replaces ONLY this file with input conversion,
//! a call to the real production board core/renderer, and output conversion.
//! No filtering, seat math, aliasing, sorting, truncation or rendering belongs
//! in the bridge. Do not synthesize expected replies from the fixtures.

use crate::contract::{Reply, Snapshot, State};

pub fn render(snapshot: &Snapshot) -> Reply {
    Reply {
        generation: snapshot.generation,
        state: State::MissingFeature,
        entries: Vec::new(),
        omitted: 0,
        content: "Live rooms board is not implemented.".to_owned(),
    }
}
