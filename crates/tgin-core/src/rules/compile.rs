//! Compiling [`RuleSpec`]s into a [`RouteTable`].

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use aho_corasick::{AhoCorasick, MatchKind, StartKind};
use regex_automata::meta::Regex;

use super::{InvalidRule, RuleError, RuleSpec, SplitStrategy, TargetSpec, TextMatch};
use crate::ids::{ConsumerId, SourceId};

/// The longest weighted-split schedule (sum of weights divided by their gcd).
pub(super) const MAX_WEIGHT_PERIOD: u64 = 1 << 16;

/// Compiled routing rules for one config generation.
///
/// Per source an ordered rule list; matcher automata per rule; one combined
/// regex per text field (each rule owns a contiguous range of its pattern
/// ids); and an arena of target nodes with lock-free split cursors.
#[derive(Debug)]
pub struct RouteTable {
    /// Indexed by `SourceId.0`.
    pub(super) sources: Vec<Option<SourceRules>>,
    pub(super) nodes: Vec<Node>,
    /// Child node indices of `All` and `Split` nodes, as contiguous ranges.
    pub(super) children: Vec<u32>,
    pub(super) text_regex: Option<Regex>,
    pub(super) callback_regex: Option<Regex>,
    /// Identity of this table; a [`super::MatchScratch`] built for another
    /// table re-warms itself.
    pub(super) token: Arc<()>,
}

#[derive(Debug)]
pub(super) struct SourceRules {
    pub(super) rules: Vec<CompiledRule>,
    /// The bot username without `@`, for `/cmd@bot` commands.
    pub(super) username: Option<Box<str>>,
}

#[derive(Debug)]
pub(super) struct CompiledRule {
    pub(super) update_types: HashSet<Box<str>>,
    pub(super) chat_ids: HashSet<i64>,
    /// Bitmask of [`crate::facts::ChatType::bit`]; 0 = any.
    pub(super) chat_types: u8,
    pub(super) user_ids: HashSet<i64>,
    pub(super) commands: Option<AhoCorasick>,
    pub(super) text: Option<TextMatcher>,
    pub(super) callback_data: Option<TextMatcher>,
    pub(super) root: u32,
}

#[derive(Debug)]
pub(super) enum TextMatcher {
    Prefix(AhoCorasick),
    /// Pattern ids in the field's combined regex.
    Regex(Range<u32>),
    Equals(HashSet<Box<str>>),
}

#[derive(Debug)]
pub(super) enum Node {
    Consumer(ConsumerId),
    All(Range<u32>),
    Split {
        strategy: SplitStrategy,
        children: Range<u32>,
        cursor: AtomicUsize,
        /// For `Weighted`: one period of the smooth weighted round robin
        /// sequence, as child positions. Empty otherwise.
        schedule: Box<[u32]>,
    },
}

/// Which text field a matcher belongs to.
#[derive(Clone, Copy)]
enum Field {
    Text,
    Callback,
}

struct Builder {
    nodes: Vec<Node>,
    children: Vec<u32>,
    text_patterns: Vec<String>,
    callback_patterns: Vec<String>,
}

impl RouteTable {
    /// Compiles the rules of every source. `bot_usernames` gives each
    /// source's bot username (with or without `@`); sources missing there
    /// never match `/cmd@bot` commands.
    pub fn compile(
        rules: &[(SourceId, Vec<RuleSpec>)],
        bot_usernames: &[(SourceId, Option<String>)],
    ) -> Result<Self, RuleError> {
        let mut b = Builder {
            nodes: Vec::new(),
            children: Vec::new(),
            text_patterns: Vec::new(),
            callback_patterns: Vec::new(),
        };
        let mut sources: Vec<Option<SourceRules>> = Vec::new();
        for (source_id, specs) in rules {
            let idx = usize::from(source_id.0);
            if sources.len() <= idx {
                sources.resize_with(idx + 1, || None);
            }
            if sources[idx].is_some() {
                return Err(RuleError::DuplicateSource(*source_id));
            }
            let username = bot_usernames
                .iter()
                .find(|(id, _)| id == source_id)
                .and_then(|(_, u)| u.as_deref())
                .map(|u| Box::from(u.strip_prefix('@').unwrap_or(u)));
            let mut compiled = Vec::with_capacity(specs.len());
            for (rule, spec) in specs.iter().enumerate() {
                compiled.push(b.rule(spec).map_err(|reason| RuleError::Invalid {
                    source_id: *source_id,
                    rule,
                    reason,
                })?);
            }
            sources[idx] = Some(SourceRules {
                rules: compiled,
                username,
            });
        }
        Ok(RouteTable {
            sources,
            nodes: b.nodes,
            children: b.children,
            text_regex: combined(&b.text_patterns)?,
            callback_regex: combined(&b.callback_patterns)?,
            token: Arc::new(()),
        })
    }
}

/// Builds one regex over every pattern of a field. `MatchKind::All` makes the
/// overlapping search report every matching pattern.
fn combined(patterns: &[String]) -> Result<Option<Regex>, RuleError> {
    if patterns.is_empty() {
        return Ok(None);
    }
    Regex::builder()
        .configure(Regex::config().match_kind(regex_automata::MatchKind::All))
        .build_many(patterns)
        .map(Some)
        .map_err(|e| RuleError::RegexSet(Box::new(e)))
}

fn anchored_automaton<I, P>(patterns: I, case_insensitive: bool) -> Result<AhoCorasick, InvalidRule>
where
    I: IntoIterator<Item = P>,
    P: AsRef<[u8]>,
{
    Ok(AhoCorasick::builder()
        .match_kind(MatchKind::LeftmostLongest)
        .start_kind(StartKind::Anchored)
        .ascii_case_insensitive(case_insensitive)
        .build(patterns)?)
}

impl Builder {
    fn rule(&mut self, spec: &RuleSpec) -> Result<CompiledRule, InvalidRule> {
        let p = &spec.predicates;
        let commands = if p.commands.is_empty() {
            None
        } else {
            let mut names = Vec::with_capacity(p.commands.len());
            for cmd in &p.commands {
                let name = cmd.strip_prefix('/').unwrap_or(cmd);
                if name.is_empty() || name.contains('@') {
                    return Err(InvalidRule::InvalidCommand(cmd.clone()));
                }
                names.push(name);
            }
            Some(anchored_automaton(names, true)?)
        };
        let text = p
            .text
            .as_ref()
            .map(|m| self.text_matcher(m, Field::Text))
            .transpose()?;
        let callback_data = p
            .callback_data
            .as_ref()
            .map(|m| self.text_matcher(m, Field::Callback))
            .transpose()?;
        let root = match spec.targets.as_slice() {
            [] => return Err(InvalidRule::NoTargets),
            [single] => self.target(single)?,
            many => {
                let children = self.group(many)?;
                self.push(Node::All(children))
            }
        };
        Ok(CompiledRule {
            update_types: p
                .update_types
                .iter()
                .map(|s| Box::from(s.as_str()))
                .collect(),
            chat_ids: p.chat_ids.iter().copied().collect(),
            chat_types: p.chat_types.iter().fold(0, |m, t| m | t.bit()),
            user_ids: p.user_ids.iter().copied().collect(),
            commands,
            text,
            callback_data,
            root,
        })
    }

    fn text_matcher(&mut self, m: &TextMatch, field: Field) -> Result<TextMatcher, InvalidRule> {
        let (TextMatch::Prefix(list) | TextMatch::Regex(list) | TextMatch::Equals(list)) = m;
        if list.is_empty() {
            return Err(InvalidRule::EmptyTextMatch);
        }
        Ok(match m {
            TextMatch::Prefix(list) => TextMatcher::Prefix(anchored_automaton(list, false)?),
            TextMatch::Equals(list) => {
                TextMatcher::Equals(list.iter().map(|s| Box::from(s.as_str())).collect())
            }
            TextMatch::Regex(list) => {
                // Validate each pattern alone so errors name the culprit.
                for pattern in list {
                    Regex::new(pattern).map_err(|e| InvalidRule::Regex {
                        pattern: pattern.clone(),
                        error: Box::new(e),
                    })?;
                }
                let patterns = match field {
                    Field::Text => &mut self.text_patterns,
                    Field::Callback => &mut self.callback_patterns,
                };
                let start = index(patterns.len());
                patterns.extend(list.iter().cloned());
                TextMatcher::Regex(start..index(patterns.len()))
            }
        })
    }

    fn push(&mut self, node: Node) -> u32 {
        self.nodes.push(node);
        index(self.nodes.len() - 1)
    }

    /// Compiles children first, then stores their indices contiguously.
    fn group(&mut self, specs: &[TargetSpec]) -> Result<Range<u32>, InvalidRule> {
        if specs.is_empty() {
            return Err(InvalidRule::EmptyGroup);
        }
        let ids = specs
            .iter()
            .map(|s| self.target(s))
            .collect::<Result<Vec<_>, _>>()?;
        let start = index(self.children.len());
        self.children.extend(ids);
        Ok(start..index(self.children.len()))
    }

    fn target(&mut self, spec: &TargetSpec) -> Result<u32, InvalidRule> {
        let node = match spec {
            TargetSpec::Consumer(id) => Node::Consumer(*id),
            TargetSpec::All(children) => Node::All(self.group(children)?),
            TargetSpec::Split {
                strategy,
                among,
                weights,
            } => {
                if among.is_empty() {
                    return Err(InvalidRule::EmptyGroup);
                }
                let schedule = match strategy {
                    SplitStrategy::Weighted => {
                        if weights.len() != among.len() {
                            return Err(InvalidRule::WeightCount {
                                weights: weights.len(),
                                children: among.len(),
                            });
                        }
                        smooth_wrr_schedule(weights)?
                    }
                    SplitStrategy::RoundRobin | SplitStrategy::HashChat => {
                        if !weights.is_empty() {
                            return Err(InvalidRule::UnexpectedWeights);
                        }
                        Box::default()
                    }
                };
                Node::Split {
                    strategy: *strategy,
                    children: self.group(among)?,
                    cursor: AtomicUsize::new(0),
                    schedule,
                }
            }
        };
        Ok(self.push(node))
    }
}

/// Converts a length to a `u32` index. Rule sets are far below `u32::MAX`
/// entries; saturating keeps this total without a panic path.
fn index(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// One period of nginx's smooth weighted round robin: each step adds every
/// weight to its current value, picks the largest (first on ties) and
/// subtracts the total from it. Weights are reduced by their gcd first; the
/// sequence then repeats every `sum(weights)` picks, so precomputing it lets
/// `route` stay lock-free (an atomic cursor indexes the schedule).
fn smooth_wrr_schedule(weights: &[u32]) -> Result<Box<[u32]>, InvalidRule> {
    let g = weights.iter().fold(0, |g, &w| gcd(g, u64::from(w)));
    if g == 0 {
        return Err(InvalidRule::ZeroTotalWeight);
    }
    let reduced: Vec<i64> = weights.iter().map(|&w| (u64::from(w) / g) as i64).collect();
    let total: i64 = reduced.iter().sum();
    let period = total as u64;
    if period > MAX_WEIGHT_PERIOD {
        return Err(InvalidRule::WeightPeriodTooLarge(period));
    }
    let mut current = vec![0_i64; reduced.len()];
    let mut schedule = Vec::with_capacity(period as usize);
    for _ in 0..period {
        let mut pick = 0;
        for (i, w) in reduced.iter().enumerate() {
            current[i] += w;
            if current[i] > current[pick] {
                pick = i;
            }
        }
        current[pick] -= total;
        schedule.push(index(pick));
    }
    Ok(schedule.into_boxed_slice())
}
