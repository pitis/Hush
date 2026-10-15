//! The app-facing view of Signal data: conversations, messages, reactions and search, kept in
//! its own encrypted SQLite file and fed from presage content.

pub mod db;
pub mod model;

pub use db::ProjectionDb;
pub use model::*;
