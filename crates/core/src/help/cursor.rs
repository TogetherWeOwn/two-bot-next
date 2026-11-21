//! Stateless help page identity. A cursor is not an authorization token.

use sha2::{Digest, Sha256};

use super::pages::{Page, MAX_HELP_PAGES};

const NAMESPACE: &str = "h1";
const MAX_ID_BYTES: usize = 71;
pub const LIFETIME_SECONDS: u64 = 600;
const MAX_EXPIRY: u64 = 9_999_999_999;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Revision([u8; 16]);

impl Revision {
    /// Identify a checked plan in one guild/application. Change `render_version`
    /// when branding, navigation, or rendering rules change.
    pub fn for_pages(guild: u64, application: u64, render_version: &str, pages: &[Page]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"help-page-plan:h1\0");
        hasher.update(guild.to_be_bytes());
        hasher.update(application.to_be_bytes());
        hash_text(&mut hasher, render_version);
        hasher.update((pages.len() as u64).to_be_bytes());
        for page in pages {
            hash_text(&mut hasher, &page.description);
            hasher.update((page.fields.len() as u64).to_be_bytes());
            for (name, value) in &page.fields {
                hash_text(&mut hasher, name);
                hash_text(&mut hasher, value);
            }
        }
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&hasher.finalize()[..16]);
        Self(bytes)
    }
}

fn hash_text(hasher: &mut Sha256, text: &str) {
    hasher.update((text.len() as u64).to_be_bytes());
    hasher.update(text.as_bytes());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cursor {
    revision: Revision,
    actor: u64,
    expires_at: u64,
    page: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorError {
    Invalid,
    Expired,
    Stale,
}

impl Cursor {
    /// Construct a cursor for a zero-based absolute target.
    pub fn new(revision: Revision, actor: u64, expires_at: u64, page: usize) -> Option<Self> {
        (actor != 0 && (1..=MAX_EXPIRY).contains(&expires_at) && page < MAX_HELP_PAGES).then_some(
            Self {
                revision,
                actor,
                expires_at,
                page,
            },
        )
    }

    /// The wire target is one-based so it matches the button label.
    pub fn encode(self) -> String {
        format!(
            "{NAMESPACE}:{}:{}:{}:{}",
            hex::encode(self.revision.0),
            self.actor,
            self.expires_at,
            self.page + 1
        )
    }

    /// Recognize the reserved component namespace, even for malformed cursors.
    pub fn is_help_control(id: &str) -> bool {
        id.strip_prefix(NAMESPACE)
            .is_some_and(|suffix| suffix.starts_with(':'))
    }

    /// Compare against the authoritative interaction actor before building a plan.
    pub const fn is_for_actor(self, actor: u64) -> bool {
        self.actor == actor
    }

    /// Decode exactly the emitted grammar. The caller still needs to validate
    /// the actual Discord source message and use the authoritative actor.
    pub fn parse(id: &str) -> Option<Self> {
        if id.len() > MAX_ID_BYTES {
            return None;
        }
        let mut parts = id.split(':');
        if parts.next()? != NAMESPACE {
            return None;
        }
        let digest = parts.next()?;
        if digest.len() != 32
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        let mut bytes = [0; 16];
        hex::decode_to_slice(digest, &mut bytes).ok()?;
        let actor = decimal(parts.next()?, 20)?.parse().ok()?;
        let expires_at = decimal(parts.next()?, 10)?.parse().ok()?;
        let page = decimal(parts.next()?, 3)?
            .parse::<usize>()
            .ok()?
            .checked_sub(1)?;
        if parts.next().is_some() {
            return None;
        }
        Self::new(Revision(bytes), actor, expires_at, page)
    }

    /// Use only after checking the component's source message and guild. The
    /// returned expiry must be kept unchanged in descendant controls.
    pub fn target(
        self,
        actor: u64,
        now: u64,
        current: Revision,
        page_count: usize,
    ) -> Result<(usize, u64), CursorError> {
        if !self.is_for_actor(actor) || self.page >= page_count {
            return Err(CursorError::Invalid);
        }
        if current != self.revision {
            return Err(CursorError::Stale);
        }
        if now >= self.expires_at || self.expires_at > now.saturating_add(LIFETIME_SECONDS) {
            return Err(CursorError::Expired);
        }
        Ok((self.page, self.expires_at))
    }
}

fn decimal(text: &str, width: usize) -> Option<&str> {
    (!text.is_empty()
        && text.len() <= width
        && (text == "0" || !text.starts_with('0'))
        && text.bytes().all(|b| b.is_ascii_digit()))
    .then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_covers_scope_rendering_and_ordered_content() {
        let first = Page {
            description: "notes".into(),
            fields: vec![("ab".into(), "c".into())],
        };
        let second = Page {
            description: "notes".into(),
            fields: vec![("a".into(), "bc".into())],
        };
        let revision = Revision::for_pages(1, 2, "v1", std::slice::from_ref(&first));
        let pages = [first, second];
        assert_eq!(revision, Revision::for_pages(1, 2, "v1", &pages[..1]));
        assert_ne!(revision, Revision::for_pages(2, 2, "v1", &pages[..1]));
        assert_ne!(revision, Revision::for_pages(1, 3, "v1", &pages[..1]));
        assert_ne!(revision, Revision::for_pages(1, 2, "v2", &pages[..1]));
        assert_ne!(revision, Revision::for_pages(1, 2, "v1", &pages[1..]));
        let ordered = Revision::for_pages(1, 2, "v1", &pages);
        let [first, second] = pages;
        assert_ne!(ordered, Revision::for_pages(1, 2, "v1", &[second, first]));
    }

    #[test]
    fn cursor_accepts_only_the_bounded_canonical_grammar() {
        let digest = "ab".repeat(16);
        let valid = Cursor::new(
            Revision([0xab; 16]),
            u64::MAX,
            MAX_EXPIRY,
            MAX_HELP_PAGES - 1,
        )
        .expect("largest cursor");
        let id = valid.encode();
        assert_eq!(id.len(), MAX_ID_BYTES);
        assert!(id.ends_with(&format!(":{MAX_HELP_PAGES}")));
        assert_eq!(Cursor::parse(&id), Some(valid));
        for malformed in [
            format!("h2:{digest}:1:2:1"),
            format!("h1:{}:1:2:1", digest.to_uppercase()),
            format!("h1:{}:1:2:1", "g".repeat(32)),
            format!("h1:{}:1:2:1", &digest[1..]),
            format!("h1:{digest}:18446744073709551616:2:1"),
            format!("h1:{digest}:0:2:1"),
            format!("h1:{digest}::2:1"),
            format!("h1:{digest}:01:2:1"),
            format!("h1:{digest}:1:+2:1"),
            format!("h1:{digest}:1:0:1"),
            format!("h1:{digest}:1:10000000000:1"),
            format!("h1:{digest}:1:2"),
            format!("h1:{digest}:1:2:1:"),
            format!("h1:{digest}:1:2:0"),
            format!("h1:{digest}:1:2:01"),
            format!("h1:{digest}:1:2:-1"),
            format!("h1:{digest}:1:2:{}", MAX_HELP_PAGES + 1),
            format!("h1:{digest}:1:2:1111"),
            format!("{id}:trailing"),
        ] {
            assert_eq!(Cursor::parse(&malformed), None, "{malformed}");
        }
    }

    #[test]
    fn target_never_paginates_another_actor_or_revision() {
        let revision = Revision([1; 16]);
        let now = 1000;
        let expires_at = now + LIFETIME_SECONDS;
        let cursor = Cursor::new(revision, 42, expires_at, 8).expect("valid cursor");
        assert_eq!(cursor.target(42, now, revision, 9), Ok((8, expires_at)));
        assert_eq!(cursor.target(42, now + 1, revision, 9), Ok((8, expires_at)));
        let changed = Revision([2; 16]);
        for (actor, time, current, count, error) in [
            (43, now, revision, 9, CursorError::Invalid),
            (43, expires_at, changed, 9, CursorError::Invalid),
            (42, now - 1, revision, 9, CursorError::Expired),
            (42, expires_at, revision, 9, CursorError::Expired),
            (42, now, changed, 9, CursorError::Stale),
            (42, expires_at, changed, 9, CursorError::Stale),
            (42, now, revision, 8, CursorError::Invalid),
            (42, expires_at, changed, 8, CursorError::Invalid),
        ] {
            assert_eq!(cursor.target(actor, time, current, count), Err(error));
        }
    }
}
