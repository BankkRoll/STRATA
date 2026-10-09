//! The shell features use the app's one command error ([`crate::error`]);
//! these names keep their signatures short.

pub use crate::error::{
    CmdResult as FeatureResult, CommandError as FeatureError, ErrorCode as ErrorKind, blocking,
};
