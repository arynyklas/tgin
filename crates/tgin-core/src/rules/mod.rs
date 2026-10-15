//! The rules engine: plain [`RuleSpec`]s compile into a [`RouteTable`] that
//! routes [`Facts`](crate::facts::Facts) to consumers without allocating.

mod compile;
mod eval;
mod spec;

pub use compile::RouteTable;
pub use eval::MatchScratch;
pub use spec::{Predicates, RuleSpec, SplitStrategy, TargetSpec, TextMatch};

use crate::ids::SourceId;

/// Why a rule set failed to compile.
#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    #[error("source {0:?} is listed more than once")]
    DuplicateSource(SourceId),
    #[error("source {source_id:?}, rule {rule}: {reason}")]
    Invalid {
        source_id: SourceId,
        rule: usize,
        reason: InvalidRule,
    },
    #[error("building the combined regex set failed: {0}")]
    RegexSet(#[source] Box<regex_automata::meta::BuildError>),
}

/// What is wrong with one rule.
#[derive(Debug, thiserror::Error)]
pub enum InvalidRule {
    #[error("the rule has no targets")]
    NoTargets,
    #[error("an `all` or `split` target has no children")]
    EmptyGroup,
    #[error("invalid command {0:?}: it must be non-empty and contain no `@`")]
    InvalidCommand(String),
    #[error("a text match has an empty list")]
    EmptyTextMatch,
    #[error("invalid regex {pattern:?}: {error}")]
    Regex {
        pattern: String,
        #[source]
        error: Box<regex_automata::meta::BuildError>,
    },
    #[error("building a matcher failed: {0}")]
    Automaton(#[from] aho_corasick::BuildError),
    #[error("a weighted split has {weights} weights for {children} children")]
    WeightCount { weights: usize, children: usize },
    #[error("weights are only allowed on weighted splits")]
    UnexpectedWeights,
    #[error("a weighted split has a zero total weight")]
    ZeroTotalWeight,
    #[error("a weighted split's period {0} exceeds {max}", max = compile::MAX_WEIGHT_PERIOD)]
    WeightPeriodTooLarge(u64),
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use smallvec::SmallVec;

    use super::*;
    use crate::facts::{ChatType, Facts};
    use crate::ids::ConsumerId;

    const S: SourceId = SourceId(0);

    fn c(n: u16) -> TargetSpec {
        TargetSpec::Consumer(ConsumerId(n))
    }

    fn rule(predicates: Predicates, targets: Vec<TargetSpec>) -> RuleSpec {
        RuleSpec {
            predicates,
            targets,
        }
    }

    fn table(rules: Vec<RuleSpec>) -> RouteTable {
        RouteTable::compile(&[(S, rules)], &[(S, Some("MyBot".to_owned()))]).expect("compile")
    }

    fn route(t: &RouteTable, f: &Facts<'_>) -> (Option<usize>, Vec<u16>) {
        let mut out = SmallVec::new();
        let mut scratch = MatchScratch::default();
        let r = t.route(S, f, &mut out, &mut scratch);
        (r, out.iter().map(|c| c.0).collect())
    }

    fn msg(text: &str) -> Facts<'_> {
        Facts {
            update_type: "message",
            chat_id: Some(10),
            chat_type: Some(ChatType::Private),
            user_id: Some(20),
            text: Some(Cow::Borrowed(text)),
            ..Facts::default()
        }
    }

    fn split(strategy: SplitStrategy, among: Vec<TargetSpec>, weights: Vec<u32>) -> TargetSpec {
        TargetSpec::Split {
            strategy,
            among,
            weights,
        }
    }

    #[test]
    fn first_match_wins() {
        let t = table(vec![
            rule(
                Predicates {
                    update_types: vec!["message".into()],
                    ..Predicates::default()
                },
                vec![c(1)],
            ),
            rule(Predicates::default(), vec![c(2)]),
        ]);
        assert_eq!(route(&t, &msg("hi")), (Some(0), vec![1]));
        let cb = Facts {
            update_type: "callback_query",
            ..Facts::default()
        };
        assert_eq!(route(&t, &cb), (Some(1), vec![2]));
    }

    #[test]
    fn empty_predicates_match_all() {
        let t = table(vec![rule(Predicates::default(), vec![c(3)])]);
        assert_eq!(route(&t, &Facts::default()), (Some(0), vec![3]));
        assert_eq!(route(&t, &msg("x")), (Some(0), vec![3]));
    }

    #[test]
    fn or_within_and_across() {
        let t = table(vec![rule(
            Predicates {
                chat_ids: vec![10, 11],
                chat_types: vec![ChatType::Private, ChatType::Group],
                user_ids: vec![20],
                ..Predicates::default()
            },
            vec![c(1)],
        )]);
        assert_eq!(route(&t, &msg("x")).0, Some(0));
        let mut f = msg("x");
        f.chat_id = Some(11);
        f.chat_type = Some(ChatType::Group);
        assert_eq!(route(&t, &f).0, Some(0));
        f.chat_type = Some(ChatType::Channel);
        assert_eq!(route(&t, &f).0, None);
        f.chat_type = Some(ChatType::Group);
        f.user_id = Some(21);
        assert_eq!(route(&t, &f).0, None);
        f.user_id = Some(20);
        f.chat_id = None;
        assert_eq!(route(&t, &f).0, None);
    }

    #[test]
    fn command_bot_suffix_rules() {
        let t = table(vec![rule(
            Predicates {
                commands: vec!["start".into(), "st".into()],
                ..Predicates::default()
            },
            vec![c(1)],
        )]);
        let with = |name, bot| Facts {
            update_type: "message",
            command: Some((name, bot)),
            ..Facts::default()
        };
        assert_eq!(route(&t, &with("start", None)).0, Some(0));
        assert_eq!(route(&t, &with("START", None)).0, Some(0));
        assert_eq!(route(&t, &with("st", None)).0, Some(0));
        assert_eq!(route(&t, &with("sta", None)).0, None);
        assert_eq!(route(&t, &with("starter", None)).0, None);
        assert_eq!(route(&t, &with("start", Some("mybot"))).0, Some(0));
        assert_eq!(route(&t, &with("start", Some("OtherBot"))).0, None);
        assert_eq!(route(&t, &msg("/start")).0, None);

        // A source without a username never matches `@bot` commands.
        let anon = RouteTable::compile(
            &[(
                S,
                vec![rule(
                    Predicates {
                        commands: vec!["start".into()],
                        ..Predicates::default()
                    },
                    vec![c(1)],
                )],
            )],
            &[],
        )
        .expect("compile");
        assert_eq!(route(&anon, &with("start", Some("mybot"))).0, None);
        assert_eq!(route(&anon, &with("start", None)).0, Some(0));

        let bad = RouteTable::compile(
            &[(
                S,
                vec![rule(
                    Predicates {
                        commands: vec!["start@bot".into()],
                        ..Predicates::default()
                    },
                    vec![c(1)],
                )],
            )],
            &[],
        );
        assert!(matches!(
            bad,
            Err(RuleError::Invalid {
                reason: InvalidRule::InvalidCommand(_),
                ..
            })
        ));
    }

    #[test]
    fn text_prefix_regex_equals() {
        let text = |m| Predicates {
            text: Some(m),
            ..Predicates::default()
        };
        let t = table(vec![
            rule(
                text(TextMatch::Prefix(vec!["buy ".into(), "sell ".into()])),
                vec![c(1)],
            ),
            rule(
                text(TextMatch::Regex(vec![r"^\d+$".into(), "ord-[a-z]+".into()])),
                vec![c(2)],
            ),
            rule(text(TextMatch::Equals(vec!["help".into()])), vec![c(3)]),
        ]);
        assert_eq!(route(&t, &msg("buy milk")).0, Some(0));
        assert_eq!(route(&t, &msg("sell")).0, None);
        assert_eq!(route(&t, &msg("12345")).0, Some(1));
        assert_eq!(route(&t, &msg("my ord-abc")).0, Some(1));
        assert_eq!(route(&t, &msg("help")).0, Some(2));
        assert_eq!(route(&t, &msg("help me")).0, None);
        assert_eq!(route(&t, &msg("Buy milk")).0, None);
        let no_text = Facts {
            update_type: "message",
            ..Facts::default()
        };
        assert_eq!(route(&t, &no_text).0, None);
    }

    #[test]
    fn callback_data_prefix() {
        let t = table(vec![
            rule(
                Predicates {
                    callback_data: Some(TextMatch::Prefix(vec!["pay:".into()])),
                    ..Predicates::default()
                },
                vec![c(1)],
            ),
            rule(
                Predicates {
                    callback_data: Some(TextMatch::Regex(vec!["^nav:[0-9]+$".into()])),
                    ..Predicates::default()
                },
                vec![c(2)],
            ),
        ]);
        let cb = |d| Facts {
            update_type: "callback_query",
            callback_data: Some(Cow::Borrowed(d)),
            ..Facts::default()
        };
        assert_eq!(route(&t, &cb("pay:42")).0, Some(0));
        assert_eq!(route(&t, &cb("nav:7")).0, Some(1));
        assert_eq!(route(&t, &cb("nav:x")).0, None);
        // Text never satisfies a callback_data predicate.
        assert_eq!(route(&t, &msg("pay:42")).0, None);
    }

    #[test]
    fn fanout_dedups_consumers() {
        let t = table(vec![rule(
            Predicates::default(),
            vec![c(1), TargetSpec::All(vec![c(2), c(1)]), c(2), c(3)],
        )]);
        assert_eq!(route(&t, &msg("x")), (Some(0), vec![1, 2, 3]));
    }

    #[test]
    fn split_round_robin_cycles() {
        let t = table(vec![rule(
            Predicates::default(),
            vec![split(
                SplitStrategy::RoundRobin,
                vec![c(1), c(2), c(3)],
                vec![],
            )],
        )]);
        let got: Vec<u16> = (0..6).flat_map(|_| route(&t, &msg("x")).1).collect();
        assert_eq!(got, [1, 2, 3, 1, 2, 3]);
    }

    #[test]
    fn split_hash_chat_sticky() {
        let t = table(vec![rule(
            Predicates::default(),
            vec![split(
                SplitStrategy::HashChat,
                vec![c(1), c(2), c(3)],
                vec![],
            )],
        )]);
        for chat in [-1001_i64, 5, 6, 7, 42] {
            let mut f = msg("x");
            f.chat_id = Some(chat);
            let first = route(&t, &f).1;
            for _ in 0..5 {
                assert_eq!(route(&t, &f).1, first);
            }
        }
        // Without a key it falls back to round robin.
        let anon = Facts::default();
        let got: Vec<u16> = (0..3).flat_map(|_| route(&t, &anon).1).collect();
        let mut sorted = got.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, [1, 2, 3]);
    }

    #[test]
    fn split_weighted_distribution_exact_over_period() {
        let t = table(vec![rule(
            Predicates::default(),
            vec![split(SplitStrategy::Weighted, vec![c(1), c(2)], vec![3, 1])],
        )]);
        for _ in 0..3 {
            let period: Vec<u16> = (0..4).flat_map(|_| route(&t, &msg("x")).1).collect();
            assert_eq!(period.iter().filter(|&&c| c == 1).count(), 3);
            assert_eq!(period.iter().filter(|&&c| c == 2).count(), 1);
            // nginx smooth WRR for 3:1 interleaves: a a b a.
            assert_eq!(period, [1, 1, 2, 1]);
        }

        let t = table(vec![rule(
            Predicates::default(),
            vec![split(
                SplitStrategy::Weighted,
                vec![c(1), c(2), c(3)],
                vec![5, 1, 1],
            )],
        )]);
        let period: Vec<u16> = (0..7).flat_map(|_| route(&t, &msg("x")).1).collect();
        assert_eq!(period, [1, 1, 2, 1, 3, 1, 1]);

        let bad = RouteTable::compile(
            &[(
                S,
                vec![rule(
                    Predicates::default(),
                    vec![split(SplitStrategy::Weighted, vec![c(1), c(2)], vec![1])],
                )],
            )],
            &[],
        );
        assert!(matches!(
            bad,
            Err(RuleError::Invalid {
                reason: InvalidRule::WeightCount { .. },
                ..
            })
        ));
    }

    #[test]
    fn nested_all_in_split() {
        let t = table(vec![rule(
            Predicates::default(),
            vec![split(
                SplitStrategy::RoundRobin,
                vec![TargetSpec::All(vec![c(1), c(2)]), c(3)],
                vec![],
            )],
        )]);
        assert_eq!(route(&t, &msg("x")).1, [1, 2]);
        assert_eq!(route(&t, &msg("x")).1, [3]);
        assert_eq!(route(&t, &msg("x")).1, [1, 2]);
    }

    #[test]
    fn unmatched_returns_none() {
        let t = table(vec![rule(
            Predicates {
                update_types: vec!["edited_message".into()],
                ..Predicates::default()
            },
            vec![c(1)],
        )]);
        assert_eq!(route(&t, &msg("x")), (None, vec![]));
        // Unknown source.
        let mut out = SmallVec::new();
        let mut scratch = MatchScratch::default();
        assert_eq!(
            t.route(SourceId(9), &msg("x"), &mut out, &mut scratch),
            None
        );
        assert!(out.is_empty());
    }

    #[test]
    fn route_does_not_allocate_after_warmup() {
        let t = table(vec![
            rule(
                Predicates {
                    text: Some(TextMatch::Regex(vec!["^zzz".into()])),
                    ..Predicates::default()
                },
                vec![c(9)],
            ),
            rule(
                Predicates {
                    callback_data: Some(TextMatch::Regex(vec!["q".into()])),
                    ..Predicates::default()
                },
                vec![c(8)],
            ),
            rule(
                Predicates::default(),
                vec![TargetSpec::All((0..8).map(c).collect())],
            ),
        ]);
        let mut out: SmallVec<[ConsumerId; 8]> = SmallVec::new();
        let mut scratch = MatchScratch::default();
        let inputs = [msg("hello"), msg("zzz"), msg("another text")];
        for f in &inputs {
            t.route(S, f, &mut out, &mut scratch);
        }
        let warm = scratch.memory_usage();
        for _ in 0..100 {
            for f in &inputs {
                t.route(S, f, &mut out, &mut scratch);
                assert!(!out.spilled());
            }
        }
        assert_eq!(scratch.memory_usage(), warm);
        assert_eq!(out.len(), 8);
    }
}
