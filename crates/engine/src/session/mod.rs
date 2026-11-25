//! The engine's public API: an [`Engine`] per database and a [`Session`]
//! per connection, running statements through the full pipeline (ADR-0031).

mod connection;
mod engine;
mod pipeline;
mod result;
mod script;
mod stream;
mod usage;

pub use connection::Session;
pub use engine::{Engine, EngineOptions};
pub use result::QueryResult;
pub use script::{ScriptEntry, ScriptReport};
pub use stream::QueryStream;
pub use usage::Usage;
