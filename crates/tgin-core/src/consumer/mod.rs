//! Consumers: where routed updates wait for downstream bots.

pub mod pull;

pub use self::pull::{PollRequest, PollResponse, PullConsumer};
