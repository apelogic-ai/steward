//! Vendor-neutral domain types shared by Steward components.

/// Stable identity for one runtime instance.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub struct RuntimeId(pub String);
