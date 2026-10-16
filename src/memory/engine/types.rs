//! Shapes shared by every stage of unified recall.

/// One retrievable unit, normalised across the stores that feed recall.
#[derive(Debug, Clone)]
pub struct Hit {
    /// Which arm produced it (`"curated"`, `"cognitive"`).
    pub arm: &'static str,
    /// Identity inside the arm: a file path, a node id. An arm that lists the
    /// same thing twice is counted once.
    pub key: String,
    /// One line naming the item.
    pub header: String,
    /// The text worth showing, already cut to a prompt-sized snippet.
    pub body: String,
    /// Unix seconds at which the knowledge was recorded or last changed, when
    /// the store knows. `None` is neutral for the recency boost.
    pub at: Option<i64>,
}

/// What recall is asked.
#[derive(Debug, Clone)]
pub struct RecallQuery {
    pub text: String,
    /// The agent folder (the `bank` every store is scoped by).
    pub folder: String,
    /// The chat this turn belongs to; curated notes written in another chat
    /// are not shown here.
    pub chat_jid: String,
    /// How many results each arm should hand to fusion.
    pub limit: usize,
}

/// A fused, scored, token-counted result.
#[derive(Debug, Clone)]
pub struct Item {
    pub hit: Hit,
    /// Final score: rank-derived relevance times the secondary boosts.
    pub score: f64,
    /// `(arm, 1-based rank)` for every list the item appeared in. Two entries
    /// means two stores agreed.
    pub sources: Vec<(&'static str, usize)>,
    pub tokens: usize,
}

/// What one arm did, for logs and for the eval harness.
#[derive(Debug, Clone)]
pub struct ArmStat {
    pub arm: &'static str,
    pub hits: usize,
    pub millis: u64,
    /// Set when the arm failed or ran out of time; its hits are then empty.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Recalled {
    pub items: Vec<Item>,
    pub arms: Vec<ArmStat>,
    /// Items folded into a higher-ranked near-identical one.
    pub merged_duplicates: usize,
    /// Items that did not fit the token budget.
    pub dropped_by_budget: usize,
    pub tokens: usize,
}
