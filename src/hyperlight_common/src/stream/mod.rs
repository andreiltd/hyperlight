// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! Byte streams shared by the host and the guest.
//!
//! [`new`] creates a stream and returns its [`StreamReader`] and [`StreamWriter`]. The ends share
//! a bounded pipe of [`DEFAULT_WINDOW`] bytes.

mod endpoint;
mod error;
mod id;
mod mux;
mod pipe;
mod record;
mod sync;

use core::num::NonZeroU32;

pub use endpoint::{StreamReader, StreamWriter, new};
pub use error::{ProtocolError, StreamError};
pub use id::StreamId;
pub use mux::Mux;
pub(crate) use mux::Transfer;
pub use record::StatusFlags;
pub(crate) use record::{BatchEncoder, Record};

// Each session sets its window size. The other limits are fixed.

/// Streams per side with a reader on the peer.
pub const MAX_STREAMS: usize = 16;
/// Readers per function call or result.
pub const MAX_TRANSFERS: usize = 16;
/// Default window size, the most unconsumed bytes per stream.
pub const DEFAULT_WINDOW: NonZeroU32 = NonZeroU32::new(16384).unwrap();
/// Task polls per progress turn.
pub const TASK_POLL_BUDGET: usize = 64;
/// Bytes copied by one copy write.
pub const MAX_COPY_WRITE: usize = 65536;

/// One side of a sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The host process.
    Host,
    /// The guest running in the VM.
    Guest,
}

impl Side {
    /// The other side.
    pub const fn peer(self) -> Self {
        match self {
            Self::Host => Self::Guest,
            Self::Guest => Self::Host,
        }
    }
}

mod sealed {
    pub trait Sealed {}

    impl Sealed for u8 {}
}

/// Item types that streams carry. Only `u8` implements it.
pub trait StreamItem: sealed::Sealed + Send + Sync + 'static {}

impl StreamItem for u8 {}
