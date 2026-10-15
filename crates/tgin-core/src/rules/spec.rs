//! Plain rule data produced by the config layer.

use crate::facts::ChatType;
use crate::ids::ConsumerId;

/// One routing rule: when the predicates hold, deliver to the targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleSpec {
    pub predicates: Predicates,
    pub targets: Vec<TargetSpec>,
}

/// Rule predicates. Values within a list are ORed, fields are ANDed; an empty
/// list (or `None`) matches anything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Predicates {
    pub update_types: Vec<String>,
    pub chat_ids: Vec<i64>,
    pub chat_types: Vec<ChatType>,
    pub user_ids: Vec<i64>,
    /// Command names without `/` and without `@bot`; ASCII case-insensitive.
    pub commands: Vec<String>,
    pub text: Option<TextMatch>,
    pub callback_data: Option<TextMatch>,
}

/// How a text field is matched. Each list is non-empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextMatch {
    /// Starts with any of these (case-sensitive).
    Prefix(Vec<String>),
    /// Any of these regexes matches somewhere in the text.
    Regex(Vec<String>),
    /// Equals any of these exactly.
    Equals(Vec<String>),
}

/// Where a matching update goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetSpec {
    Consumer(ConsumerId),
    /// Every child receives the update.
    All(Vec<TargetSpec>),
    /// Exactly one child receives the update.
    Split {
        strategy: SplitStrategy,
        among: Vec<TargetSpec>,
        /// One weight per child for [`SplitStrategy::Weighted`]; empty otherwise.
        weights: Vec<u32>,
    },
}

/// How a [`TargetSpec::Split`] picks a child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitStrategy {
    RoundRobin,
    /// Sticky by [`crate::facts::Facts::ordering_key`]; round robin without a key.
    HashChat,
    /// Smooth weighted round robin.
    Weighted,
}
