//! Pure domain values and read-time provenance. Clocks, randomness, and storage live upstream.

mod error;
mod fact;
mod governance;
mod ids;
pub mod ingest;
mod llm;
mod provenance;
mod redactor;
mod rsi;
mod schema;
mod semiring;
pub mod time;

#[cfg(test)]
mod test_support;

pub use error::{KpError, Result};
pub use fact::*;
pub use governance::*;
pub use ids::*;
pub use ingest::*;
pub use llm::*;
pub use schema::*;
pub use semiring::*;
pub use time::*;

pub use provenance::{derived_fact, justification, slot_conflict};
pub use redactor::SecretRedactor;
pub use rsi::*;
