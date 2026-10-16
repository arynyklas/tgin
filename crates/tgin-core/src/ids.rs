//! Engine identifiers of sources and consumers.

/// The engine's id of a bot's source. It is assigned by name and stays the
/// same across reloads that keep the bot; a new bot gets an id never used
/// before, so ids are sparse and never reused. It is not the bot's position
/// in `Config::bots`: look it up with
/// [`Engine::source_id`](crate::engine::Engine::source_id).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub u16);

/// Index of a downstream consumer within one config generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConsumerId(pub u16);
