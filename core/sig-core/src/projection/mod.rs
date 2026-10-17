//! The app-facing view of Signal data: conversations, messages, reactions and search, kept in
//! its own encrypted SQLite file and fed from presage content.

pub mod chat_colors;
pub mod db;
pub mod ids;
pub mod model;
pub mod projector;

pub use db::{Expired, GroupStanding, HistoryDbStage, HistoryImportState, ProjectionDb};
pub use ids::conversation_id;
pub use model::*;
pub use projector::{project, Op};
