//! Source adapters: each turns external material into `EpisodeDraft`s.
//! Adapters never touch the store — the daemon applies their batches
//! (design law #4).

pub mod apple_health;
pub mod chatlog;
pub mod chrome;
pub mod claude_code;
pub mod facebook;
pub mod git;
pub mod gmail_imap;
pub mod ics;
pub mod imessage;
pub mod linkedin;
pub mod mbox;
pub mod openai;
pub mod photos;
pub mod rewind;
pub mod substack;
