//! Device-tree boot object stub.
//!
//! This module is a placeholder for the future boot-oriented device-tree layer.
//! It is intentionally separate from the low-level flattened-device-tree parser
//! so boot methods can depend on one stable DTB object type.

/// Boot-oriented device-tree object passed to boot methods.
pub struct Dtb {
    /// Placeholder field until boot-time DTB editing is implemented.
    _private: (),
}

impl Dtb {
    /// Creates one empty device-tree stub.
    ///
    /// # Parameters
    ///
    /// This function does not accept parameters.
    pub const fn new() -> Self {
        Self { _private: () }
    }
}