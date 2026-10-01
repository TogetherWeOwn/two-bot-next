//! Secret values are opaque to formatting; access is explicit at use sites.

use std::fmt;

/// A credential or credential-bearing URL that must not reach diagnostics.
///
/// There is deliberately no `Deref`, `AsRef`, or serialization implementation.
/// Full URL redaction also hides userinfo, query credentials and webhook paths.
/// This is a formatting boundary, not encrypted storage or memory zeroization.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Expose only to the consumer that needs the value, never to diagnostics.
    #[must_use]
    pub const fn expose(&self) -> &T {
        &self.0
    }
}

impl<T> From<T> for Secret<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_is_constant_for_every_value_and_format() {
        for value in [
            "",
            "fixture-token",
            "postgres://fixture-user:fixture-password@localhost/db",
            "https://discord.invalid/api/webhooks/1/fixture-webhook?key=fixture-query",
        ] {
            let secret = Secret::new(value.to_owned());
            for output in [
                format!("{secret}"),
                format!("{secret:?}"),
                format!("{secret:#?}"),
                format!("{secret:>100}"),
                format!("{secret:.3}"),
            ] {
                assert_eq!(output, "[REDACTED]");
            }
            assert_eq!(secret.expose(), value);
        }
    }

    #[test]
    fn formatting_never_calls_inner_formatters() {
        struct Unprintable;
        let secret = Secret::new(Unprintable);
        assert_eq!(format!("{secret:?} {secret}"), "[REDACTED] [REDACTED]");
        assert_eq!(format!("{:?}", Secret::new(vec![1_u8, 2, 3])), "[REDACTED]");
        assert_eq!(format!("{:?}", Some(secret)), "Some([REDACTED])");
    }
}
