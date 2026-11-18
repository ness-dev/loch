//! loch as a library: every module the binary is built from, public so the
//! integration tests in `tests/` can reach them.

// The error path is cold and runs once per process; boxing it buys nothing.
#![allow(clippy::result_large_err)]

pub mod argparse;
pub mod blame_html;
pub mod bundle;
pub mod claude;
pub mod cli;
pub mod format;
pub mod git;
pub mod hook;
pub mod index;
pub mod ingest;
pub mod json;
pub mod output;
pub mod py;
pub mod redact;
pub mod rewrite;
pub mod s3;
pub mod store;
pub mod sys;
pub mod trace;
