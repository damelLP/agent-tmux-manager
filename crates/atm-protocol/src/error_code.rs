//! Standard control-plane error codes. The wire accepts other string codes too.

/// The daemon does not implement the requested method.
pub const UNKNOWN_METHOD: &str = "unknown_method";
/// The method parameters are invalid.
pub const INVALID_PARAMS: &str = "invalid_params";
/// The requested resource does not exist.
pub const NOT_FOUND: &str = "not_found";
/// The operation conflicts with current state.
pub const CONFLICT: &str = "conflict";
/// The operation was refused.
pub const REFUSED: &str = "refused";
/// The operation exceeded its deadline.
pub const TIMEOUT: &str = "timeout";
/// A required service or resource is unavailable.
pub const UNAVAILABLE: &str = "unavailable";
/// An internal error prevented the operation.
pub const INTERNAL: &str = "internal";
