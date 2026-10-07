//! Permanent registration point. Registration alone never activates a family.
use crate::dispatch::FamilySpec;

/// Bootstrap deliberately registers no application handlers.
pub fn register() -> Vec<FamilySpec> {
    Vec::new()
}
