//! Messenger client core (S5, ADR 0013), platform-neutral and UI-free:
//!
//! * [`api`] — typed HTTP client of `/v1/...` (protobuf bodies);
//! * [`cache`] — SQLite cache with migrations and corrupted-file recovery;
//! * [`engine`] — sync: change feed, dedup/gap handling of events, pending operations,
//!   receipts, views for the UI;
//! * [`service`] — worker thread with its own signed-in WebSocket and reconnects.

pub mod api;
pub mod cache;
pub mod engine;
pub mod service;

pub use engine::{Changes, ConversationView, Engine, MessageState, MessageView, ReplyView};
pub use service::{Command, Service, ServiceConfig, Status, Update};
