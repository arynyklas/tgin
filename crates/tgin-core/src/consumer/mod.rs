//! Consumers: where routed updates wait for downstream bots.

pub mod pull;
pub mod push;

pub use self::pull::{PollRequest, PollResponse, PullConsumer};
pub use self::push::PushConsumer;
