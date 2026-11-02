//! Healthy-path compatibility while deferred boundaries become fallible.
//! A legacy value remains success; a new refusal always fails the control.

use std::fmt::Debug;

pub trait Healthy {
    type Value;
    fn healthy(self) -> Self::Value;
}

impl<T, E: Debug> Healthy for Result<T, E> {
    type Value = T;
    fn healthy(self) -> T {
        self.expect("healthy deferred boundary must succeed")
    }
}

impl<T> Healthy for Vec<T> {
    type Value = Self;
    fn healthy(self) -> Self {
        self
    }
}

macro_rules! legacy_value {
    ($($kind:ty),+ $(,)?) => {$(
        impl Healthy for $kind {
            type Value = Self;
            fn healthy(self) -> Self { self }
        }
    )+};
}
legacy_value!((), usize, bool);
