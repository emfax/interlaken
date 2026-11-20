//! The engine's public API: an [`Engine`] per database and a [`Session`]
//! per connection, running statements through the full pipeline (ADR-0031).

mod connection;
mod engine;
mod pipeline;
mod result;
mod stream;

pub use connection::Session;
pub use engine::{Engine, EngineOptions};
pub use result::QueryResult;
pub use stream::QueryStream;
