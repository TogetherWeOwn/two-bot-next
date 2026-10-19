//! Process-local admission, independent of the durable event replay fences.

use std::{collections::HashMap, time::Duration};
use tokio::time::Instant;

pub(crate) const COOLDOWN: Duration = Duration::from_secs(5);
const MAX_GUILDS: usize = 16;
const MAX_ACTORS_PER_GUILD: usize = 1024;

#[derive(Debug, Default)]
pub(crate) struct ActorCooldowns {
    guilds: HashMap<u64, HashMap<u64, (u64, Instant)>>,
}

impl ActorCooldowns {
    /// Refused events and retries do not extend the fixed window. A retry of
    /// the admitted event must still reach the service's durable replay guard.
    /// Never evict a live actor to admit a new one.
    pub(crate) fn admit(&mut self, guild: u64, actor: u64, event: u64, now: Instant) -> bool {
        self.guilds.retain(|_, actors| {
            actors.retain(|_, (_, until)| *until > now);
            !actors.is_empty()
        });
        if !self.guilds.contains_key(&guild) && self.guilds.len() >= MAX_GUILDS {
            return false;
        }
        let actors = self.guilds.entry(guild).or_default();
        if let Some((previous, _)) = actors.get(&actor) {
            return *previous == event;
        }
        if actors.len() >= MAX_ACTORS_PER_GUILD {
            return false;
        }
        actors.insert(actor, (event, now + COOLDOWN));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn fixed_window_allows_same_event_without_extending_on_retry_or_refusal() {
        let mut cooldowns = ActorCooldowns::default();
        assert!(cooldowns.admit(1, 2, 3, Instant::now()));
        tokio::time::advance(COOLDOWN - Duration::from_millis(1)).await;
        for _ in 0..1000 {
            assert!(cooldowns.admit(1, 2, 3, Instant::now()));
            assert!(!cooldowns.admit(1, 2, 4, Instant::now()));
        }
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(cooldowns.admit(1, 2, 4, Instant::now()));
        assert!(!cooldowns.admit(1, 2, 3, Instant::now()));
    }

    #[test]
    fn actors_guilds_and_surface_instances_are_independent() {
        let now = Instant::now();
        let mut cooldowns = ActorCooldowns::default();
        assert!(cooldowns.admit(1, 2, 3, now));
        assert!(!cooldowns.admit(1, 2, 4, now));
        assert!(cooldowns.admit(1, 5, 4, now));
        assert!(cooldowns.admit(6, 2, 4, now));
        assert!(ActorCooldowns::default().admit(1, 2, 4, now));
    }

    #[tokio::test(start_paused = true)]
    async fn capacity_fails_closed_without_evicting_live_keys_and_expires() {
        let mut cooldowns = ActorCooldowns::default();
        for guild in 1..=MAX_GUILDS as u64 {
            for actor in 1..=MAX_ACTORS_PER_GUILD as u64 {
                assert!(cooldowns.admit(guild, actor, 1, Instant::now()));
            }
            for actor in MAX_ACTORS_PER_GUILD as u64 + 1..MAX_ACTORS_PER_GUILD as u64 + 100 {
                assert!(!cooldowns.admit(guild, actor, 1, Instant::now()));
            }
            assert!(cooldowns.admit(guild, 1, 1, Instant::now()));
            assert!(!cooldowns.admit(guild, 1, 2, Instant::now()));
        }
        assert!(!cooldowns.admit(MAX_GUILDS as u64 + 1, 1, 1, Instant::now()));
        assert_eq!(cooldowns.guilds.len(), MAX_GUILDS);
        assert_eq!(cooldowns.guilds[&1].len(), MAX_ACTORS_PER_GUILD);
        tokio::time::advance(COOLDOWN).await;
        assert!(cooldowns.admit(MAX_GUILDS as u64 + 1, 1, 2, Instant::now()));
        assert_eq!(cooldowns.guilds.len(), 1);
        assert_eq!(
            cooldowns.guilds.values().map(HashMap::len).sum::<usize>(),
            1
        );
    }
}
