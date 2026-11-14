//! Importers. Each feeds raw messages to [`crate::archive::ingest`].

pub mod eml;
pub mod imap;
pub mod mapi;
pub mod mbox;
pub mod msg;
pub mod pst;
