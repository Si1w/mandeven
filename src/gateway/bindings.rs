//! Binding table — maps `(channel, peer, guild, account)` tuples to
//! an `agent_id`.
//!
//! Lifted from `agent-examples/claw0`'s Gateway design: five tiers
//! from most-specific (peer) to most-general (default), linear scan,
//! first match wins.
//!
//! In the current single-agent build the table is unused — every
//! message implicitly routes to the one agent registered in the
//! process. The type lives here so multi-agent work can drop into
//! place without restructuring the gateway.

/// Tier levels used by the binding table. Lower number = more
/// specific. Matches claw0 ordering so the concepts map 1:1 when
/// multi-agent support is wired in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierLevel {
    /// Tier 1 — specific user identity (claw0's `peer_id`).
    Peer,
    /// Tier 2 — guild / server identity.
    Guild,
    /// Tier 3 — bot / workspace account identity.
    Account,
    /// Tier 4 — channel type (e.g. `"cli"`, `"telegram"`).
    Channel,
    /// Tier 5 — catch-all fallback.
    Default,
}

impl TierLevel {
    /// Numeric tier value used for sorting; matches claw0's 1..=5.
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Self::Peer => 1,
            Self::Guild => 2,
            Self::Account => 3,
            Self::Channel => 4,
            Self::Default => 5,
        }
    }
}

/// One entry in the binding table.
#[derive(Debug, Clone)]
pub struct Binding {
    /// Agent this binding routes to.
    pub agent_id: String,
    /// Which identity dimension this binding matches on.
    pub tier: TierLevel,
    /// The concrete value to match against that dimension (for
    /// example `"cli"` with `TierLevel::Channel`, or
    /// `"discord:12345"` with `TierLevel::Peer`).
    pub match_value: String,
    /// Tie-breaker within the same tier; higher wins.
    pub priority: i32,
}

/// Ordered list of bindings, sorted by `(tier, -priority)`.
///
/// Empty in the current build. `dispatch` always returns `None`; the
/// gateway treats `None` as "fall back to the singleton agent".
#[derive(Debug, Default)]
pub struct BindingTable {
    entries: Vec<Binding>,
}

impl BindingTable {
    /// Construct an empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a binding, re-sorting by `(tier, -priority)` so scans
    /// hit the most specific entry first.
    pub fn add(&mut self, binding: Binding) {
        self.entries.push(binding);
        self.entries.sort_by_key(|b| (b.tier.rank(), -b.priority));
    }

    /// Route an identity tuple to an agent. Walks entries
    /// top-to-bottom; first match wins.
    ///
    /// Inputs are `Option<&str>` because not every channel carries
    /// every identity dimension (cron has no peer, the CLI has no
    /// guild/account, etc.).
    #[must_use]
    pub fn dispatch(
        &self,
        channel: Option<&str>,
        account: Option<&str>,
        guild: Option<&str>,
        peer: Option<&str>,
    ) -> Option<&str> {
        for b in &self.entries {
            let matched = match b.tier {
                TierLevel::Peer => peer.is_some_and(|p| b.match_value == p),
                TierLevel::Guild => guild.is_some_and(|g| b.match_value == g),
                TierLevel::Account => account.is_some_and(|a| b.match_value == a),
                TierLevel::Channel => channel.is_some_and(|c| b.match_value == c),
                TierLevel::Default => true,
            };
            if matched {
                return Some(&b.agent_id);
            }
        }
        None
    }
}
