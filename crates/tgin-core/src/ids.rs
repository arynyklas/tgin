//! Dense identifiers assigned per config generation.

/// Index of an ingress source within one config generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub u16);

/// Index of a downstream consumer within one config generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConsumerId(pub u16);
