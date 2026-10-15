//! Routing one update through a compiled [`RouteTable`].

use std::sync::Arc;
use std::sync::atomic::Ordering;

use aho_corasick::{AhoCorasick, Anchored};
use regex_automata::meta::{Cache, Regex};
use regex_automata::{Input, PatternID, PatternSet};
use smallvec::SmallVec;

use super::RouteTable;
use super::compile::{CompiledRule, Node, SourceRules, TextMatcher};
use super::spec::SplitStrategy;
use crate::facts::Facts;
use crate::ids::{ConsumerId, SourceId};

/// Per-worker reusable state for regex matching. Keep one per thread and pass
/// it to every [`RouteTable::route`] call; it warms itself on first use with a
/// table (and again after a reload swaps in a new table).
#[derive(Debug, Default)]
pub struct MatchScratch {
    owner: Option<Arc<()>>,
    text: Option<RegexScratch>,
    callback: Option<RegexScratch>,
}

#[derive(Debug)]
struct RegexScratch {
    cache: Cache,
    set: PatternSet,
    /// Whether `set` holds the result for the current update.
    done: bool,
}

impl RegexScratch {
    fn new(re: &Regex) -> Self {
        RegexScratch {
            cache: re.create_cache(),
            set: PatternSet::new(re.pattern_len()),
            done: false,
        }
    }
}

impl MatchScratch {
    /// Heap bytes held by the scratch (regex caches and pattern sets).
    pub fn memory_usage(&self) -> usize {
        [&self.text, &self.callback]
            .into_iter()
            .flatten()
            .map(|s| s.cache.memory_usage() + s.set.capacity() * size_of::<bool>())
            .sum()
    }

    fn prepare(&mut self, table: &RouteTable) {
        if !self
            .owner
            .as_ref()
            .is_some_and(|o| Arc::ptr_eq(o, &table.token))
        {
            self.owner = Some(table.token.clone());
            self.text = table.text_regex.as_ref().map(RegexScratch::new);
            self.callback = table.callback_regex.as_ref().map(RegexScratch::new);
        }
        for s in [&mut self.text, &mut self.callback].into_iter().flatten() {
            s.done = false;
        }
    }
}

impl RouteTable {
    /// Routes one update: finds the first matching rule of `source`, walks its
    /// targets and writes the deduplicated consumers to `out` (cleared first).
    /// Returns the rule's index, or `None` when no rule matches.
    pub fn route(
        &self,
        source: SourceId,
        facts: &Facts<'_>,
        out: &mut SmallVec<[ConsumerId; 8]>,
        scratch: &mut MatchScratch,
    ) -> Option<usize> {
        out.clear();
        let rules = self.sources.get(usize::from(source.0))?.as_ref()?;
        scratch.prepare(self);
        let index = rules
            .rules
            .iter()
            .position(|rule| self.matches(rules, rule, facts, scratch))?;
        self.walk(rules.rules[index].root, facts.ordering_key(), out);
        Some(index)
    }

    fn matches(
        &self,
        source: &SourceRules,
        rule: &CompiledRule,
        facts: &Facts<'_>,
        scratch: &mut MatchScratch,
    ) -> bool {
        (rule.update_types.is_empty() || rule.update_types.contains(facts.update_type))
            && (rule.chat_ids.is_empty()
                || facts.chat_id.is_some_and(|id| rule.chat_ids.contains(&id)))
            && (rule.chat_types == 0
                || facts
                    .chat_type
                    .is_some_and(|t| rule.chat_types & t.bit() != 0))
            && (rule.user_ids.is_empty()
                || facts.user_id.is_some_and(|id| rule.user_ids.contains(&id)))
            && rule
                .commands
                .as_ref()
                .is_none_or(|ac| command_matches(ac, source.username.as_deref(), facts.command))
            && rule.text.as_ref().is_none_or(|m| {
                text_matches(
                    m,
                    facts.text.as_deref(),
                    self.text_regex.as_ref(),
                    &mut scratch.text,
                )
            })
            && rule.callback_data.as_ref().is_none_or(|m| {
                text_matches(
                    m,
                    facts.callback_data.as_deref(),
                    self.callback_regex.as_ref(),
                    &mut scratch.callback,
                )
            })
    }

    fn walk(&self, node: u32, key: Option<u64>, out: &mut SmallVec<[ConsumerId; 8]>) {
        let Some(node) = self.nodes.get(node as usize) else {
            return;
        };
        match node {
            Node::Consumer(id) => {
                if !out.contains(id) {
                    out.push(*id);
                }
            }
            Node::All(children) => {
                for &child in self.child_slice(children) {
                    self.walk(child, key, out);
                }
            }
            Node::Split {
                strategy,
                children,
                cursor,
                schedule,
            } => {
                let children = self.child_slice(children);
                let len = children.len();
                if len == 0 {
                    return;
                }
                let next = || cursor.fetch_add(1, Ordering::Relaxed);
                // Hash splits consume the key in mixed radix: pick `key % len`
                // and hand `key / len` to the child, so nested hash splits are
                // decorrelated while every key stays sticky (and stable across
                // reloads of the same tree shape).
                let (pick, child_key) = match (strategy, key) {
                    (SplitStrategy::HashChat, Some(key)) => {
                        let len = len as u64;
                        ((key % len) as usize, Some(key / len))
                    }
                    (SplitStrategy::Weighted, _) if !schedule.is_empty() => {
                        (schedule[next() % schedule.len()] as usize, key)
                    }
                    _ => (next() % len, key),
                };
                if let Some(&child) = children.get(pick) {
                    self.walk(child, child_key, out);
                }
            }
        }
    }

    fn child_slice(&self, range: &std::ops::Range<u32>) -> &[u32] {
        self.children
            .get(range.start as usize..range.end as usize)
            .unwrap_or(&[])
    }
}

/// A command matches when its name equals one of the rule's (ASCII
/// case-insensitive) and any `@bot` suffix names the source's bot.
fn command_matches(
    ac: &AhoCorasick,
    username: Option<&str>,
    command: Option<(&str, Option<&str>)>,
) -> bool {
    let Some((name, bot)) = command else {
        return false;
    };
    if let Some(bot) = bot
        && !username.is_some_and(|u| u.eq_ignore_ascii_case(bot))
    {
        return false;
    }
    // Anchored + leftmost-longest: the longest rule command that prefixes
    // `name`; it is a match only if it covers all of `name`.
    ac.find(aho_corasick::Input::new(name).anchored(Anchored::Yes))
        .is_some_and(|m| m.end() == name.len())
}

fn text_matches(
    m: &TextMatcher,
    text: Option<&str>,
    regex: Option<&Regex>,
    scratch: &mut Option<RegexScratch>,
) -> bool {
    let Some(text) = text else {
        return false;
    };
    match m {
        TextMatcher::Prefix(ac) => ac
            .find(aho_corasick::Input::new(text).anchored(Anchored::Yes))
            .is_some(),
        TextMatcher::Equals(set) => set.contains(text),
        TextMatcher::Regex(ids) => {
            let (Some(re), Some(s)) = (regex, scratch.as_mut()) else {
                return false;
            };
            // One overlapping search per field and update, shared by all rules.
            if !s.done {
                s.set.clear();
                re.which_overlapping_matches_with(&mut s.cache, &Input::new(text), &mut s.set);
                s.done = true;
            }
            ids.clone()
                .any(|id| PatternID::new(id as usize).is_ok_and(|p| s.set.contains(p)))
        }
    }
}
