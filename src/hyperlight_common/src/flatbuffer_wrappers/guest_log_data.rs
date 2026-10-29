// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 The Hyperlight Authors.

use alloc::vec::Vec;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::guest_log_level::LogLevel;
use super::wire::{decode_value, encode_value};

/// A guest log record. Strings borrow from the sender's storage when encoded
/// and from the control data when decoded. Field order is the wire order.
#[derive(Eq, PartialEq, Debug, Clone, Copy, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct GuestLogData<'a> {
    pub message: &'a str,
    pub source: &'a str,
    pub level: LogLevel,
    pub caller: &'a str,
    pub source_file: &'a str,
    pub line: u32,
}

impl<'a> GuestLogData<'a> {
    /// Encode control data into `buffer`.
    pub fn encode<'b>(&self, buffer: &'b mut Vec<u8>) -> Result<&'b [u8]> {
        encode_value(self, buffer)
    }

    /// Decode control data. Strings borrow from `value`.
    pub fn decode(value: &'a [u8]) -> Result<Self> {
        decode_value(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> GuestLogData<'static> {
        GuestLogData {
            message: "message",
            source: "source",
            level: LogLevel::Warning,
            caller: "caller",
            source_file: "file.rs",
            line: 42,
        }
    }

    #[test]
    fn wire_bytes_are_stable() {
        // Changing these bytes changes the wire format and needs a snapshot ABI bump.
        let log = GuestLogData {
            message: "m",
            source: "s",
            level: LogLevel::Warning,
            caller: "c",
            source_file: "f",
            line: 300,
        };
        let mut buffer = Vec::new();
        let encoded = log.encode(&mut buffer).unwrap();

        #[rustfmt::skip]
        let expected = [
            1, b'm', // message
            1, b's', // source
            3,       // LogLevel::Warning
            1, b'c', // caller
            1, b'f', // source_file
            0xac, 2, // line, varint 300
        ];
        assert_eq!(encoded, expected);
    }

    #[test]
    fn round_trips_and_borrows() {
        let log = sample();
        let mut buffer = Vec::new();
        let encoded = log.encode(&mut buffer).unwrap();
        let decoded = GuestLogData::decode(encoded).unwrap();

        assert_eq!(decoded, log);
        assert!(encoded.as_ptr_range().contains(&decoded.message.as_ptr()));
    }

    #[test]
    fn rejects_unknown_level() {
        let mut buffer = Vec::new();
        let mut encoded = sample().encode(&mut buffer).unwrap().to_vec();
        let level = 1 + "message".len() + 1 + "source".len();
        assert_eq!(encoded[level], LogLevel::Warning as u8);
        encoded[level] = LogLevel::None as u8 + 1;

        assert!(GuestLogData::decode(&encoded).is_err());
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut buffer = Vec::new();
        let mut encoded = sample().encode(&mut buffer).unwrap().to_vec();
        encoded.push(0);

        assert!(GuestLogData::decode(&encoded).is_err());
    }
}
