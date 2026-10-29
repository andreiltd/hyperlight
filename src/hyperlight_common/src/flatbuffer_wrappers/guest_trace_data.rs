// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 The Hyperlight Authors.

//! Guest trace events and their batch encoding.
//!
//! A batch is a sequence of postcard events. Each event marks its own end.
//! Strings borrow from the guest's static metadata when encoded and from the
//! batch when decoded.

use alloc::borrow::Cow;
use alloc::vec::Vec;

use anyhow::{Error, Result, anyhow};
use serde::{Deserialize, Serialize};

use super::wire::encode_value;

/// TODO: Change these constant to be configurable at runtime by the guest
/// Maybe use a weak symbol that the guest can override at link time?
///
/// Pre-calculated capacity for the encoder buffer
/// This is to avoid reallocations in the guest
/// If the next event would exceed this size, the encoder will flush the current buffer to the host
/// before encoding the new event.
pub const MAX_TRACE_DATA_SIZE: usize = 4096;

/// Key-Value pair structure used in tracing spans/events
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventKeyValue<'a> {
    /// Key of the key-value pair
    #[serde(borrow)]
    pub key: Cow<'a, str>,
    /// Value of the key-value pair
    #[serde(borrow)]
    pub value: Cow<'a, str>,
}

/// Enum representing different types of guest events for tracing
/// such as opening/closing spans and logging events.
///
/// Variant order is the wire tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GuestEvent<'a> {
    /// Event representing the opening of a new tracing span.
    OpenSpan {
        /// Unique identifier for the span.
        /// This ID is used to correlate open and close events.
        /// It should be unique within the context of a sandboxed guest execution.
        id: u64,
        /// Optional parent span ID, if this span is nested within another span.
        parent_id: Option<u64>,
        /// Name of the span.
        #[serde(borrow)]
        name: Cow<'a, str>,
        /// Target associated with the span.
        #[serde(borrow)]
        target: Cow<'a, str>,
        /// Timestamp Counter (TSC) value when the span was opened.
        tsc: u64,
        /// Additional key-value fields associated with the span.
        #[serde(borrow)]
        fields: Vec<EventKeyValue<'a>>,
    },
    /// Event representing the closing of a tracing span.
    CloseSpan {
        /// Unique identifier for the span being closed.
        id: u64,
        /// Timestamp Counter (TSC) value when the span was closed.
        tsc: u64,
    },
    /// Event representing a log entry within a tracing span.
    LogEvent {
        /// Identifier of the parent span for this log event.
        parent_id: u64,
        /// Name of the log event.
        #[serde(borrow)]
        name: Cow<'a, str>,
        /// Timestamp Counter (TSC) value when the log event occurred.
        tsc: u64,
        /// Additional key-value fields associated with the log event.
        #[serde(borrow)]
        fields: Vec<EventKeyValue<'a>>,
    },
    /// Event representing an edit to an existing span.
    /// Corresponds to the `record` method in the tracing subscriber trait.
    EditSpan {
        /// Unique identifier for the span to edit.
        id: u64,
        /// Fields to add or modify in the span.
        #[serde(borrow)]
        fields: Vec<EventKeyValue<'a>>,
    },
    /// Event representing the start of the guest environment.
    GuestStart {
        /// Timestamp Counter (TSC) value when the guest started.
        tsc: u64,
    },
}

/// Trait defining the interface for encoding guest events.
/// Implementors of this trait should provide methods for encoding events,
/// finishing the encoding process, flushing the buffer, and resetting the encoder.
pub trait EventsEncoder {
    /// Encode a single guest event into the encoder's buffer.
    fn encode(&mut self, event: &GuestEvent<'_>);
    /// Finalize the encoding process and return the serialized buffer.
    fn finish(&self) -> &[u8];
    /// Flush the encoder's buffer, typically sending or processing the data.
    fn flush(&mut self);
    /// Reset the encoder's internal state, clearing any buffered data.
    fn reset(&mut self);
}

/// Trait defining the interface for decoding guest events.
/// Implementors of this trait should provide methods for decoding a buffer
/// of bytes into a collection of guest events.
pub trait EventsDecoder {
    /// Decode a buffer of bytes into guest events that borrow from it.
    fn decode<'a>(&self, buffer: &'a [u8]) -> Result<Vec<GuestEvent<'a>>, Error>;
}

/// Decodes a batch and rejects truncated events.
pub struct EventsBatchDecoder;

impl EventsDecoder for EventsBatchDecoder {
    fn decode<'a>(&self, data: &'a [u8]) -> Result<Vec<GuestEvent<'a>>, Error> {
        let mut rest = data;
        let mut events = Vec::new();

        while !rest.is_empty() {
            let (event, tail) = postcard::take_from_bytes(rest)
                .map_err(|e| anyhow!("Failed to decode guest trace event: {e}"))?;
            events.push(event);
            rest = tail;
        }
        Ok(events)
    }
}

pub type EventsBatchEncoder = EventsBatchEncoderGeneric<fn(&[u8])>;

/// Encoder for batching and serializing guest events into a buffer.
/// When the buffer reaches its capacity, the provided `report_full` callback
/// is invoked with the current buffer contents.
///
/// This encoder is a lossless encoder; no events are dropped.
pub struct EventsBatchEncoderGeneric<T: Fn(&[u8])> {
    /// Internal buffer for serialized events
    buffer: Vec<u8>,
    /// Reused storage for one serialized event
    scratch: Vec<u8>,
    /// Maximum capacity of the buffer
    capacity: usize,
    /// Callback function to report when the buffer is full
    report_full: T,
}

impl<T: Fn(&[u8])> EventsBatchEncoderGeneric<T> {
    /// Create a new EventsBatchEncoder with the specified initial capacity
    pub fn new(initial_capacity: usize, report_full: T) -> Self {
        Self {
            buffer: Vec::with_capacity(initial_capacity),
            scratch: Vec::new(),
            capacity: initial_capacity,
            report_full,
        }
    }
}

impl<T: Fn(&[u8])> EventsEncoder for EventsBatchEncoderGeneric<T> {
    /// Serialize a single GuestEvent and append it to the internal buffer.
    /// If the appending of the serialized data exceeds buffer capacity, the
    /// `report_full` callback is invoked with the current buffer contents,
    /// and the buffer is cleared for new data.
    fn encode(&mut self, event: &GuestEvent<'_>) {
        let Self {
            buffer,
            scratch,
            capacity,
            report_full,
        } = self;

        // Encoding into a growable vector does not fail.
        let Ok(serialized) = encode_value(event, scratch) else {
            return;
        };

        if !buffer.is_empty() && buffer.len() + serialized.len() > *capacity {
            report_full(buffer);
            buffer.clear();
        }
        buffer.extend_from_slice(serialized);
    }

    /// Get a reference to the internal buffer containing serialized events.
    /// This buffer can be sent or processed as needed.
    fn finish(&self) -> &[u8] {
        &self.buffer
    }

    /// Flush the internal buffer by invoking the `report_full` callback
    /// with the current buffer contents, then resetting the buffer.
    fn flush(&mut self) {
        if !self.buffer.is_empty() {
            (self.report_full)(&self.buffer);
            self.reset();
        }
    }

    /// Reset the internal buffer, clearing all serialized data.
    /// This prepares the encoder for new events.
    fn reset(&mut self) {
        self.buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use core::cell::RefCell;

    use super::*;

    #[test]
    fn event_wire_bytes_are_stable() {
        // Changing these bytes changes the wire format and needs a snapshot ABI bump.
        let field = || EventKeyValue {
            key: Cow::Borrowed("k"),
            value: Cow::Borrowed("v"),
        };
        #[rustfmt::skip]
        let cases: [(GuestEvent<'static>, &[u8]); 5] = [
            (
                GuestEvent::OpenSpan {
                    id: 2,
                    parent_id: Some(1),
                    name: Cow::Borrowed("n"),
                    target: Cow::Borrowed("t"),
                    tsc: 300,
                    fields: vec![field()],
                },
                &[0, 2, 1, 1, 1, b'n', 1, b't', 0xac, 2, 1, 1, b'k', 1, b'v'],
            ),
            (GuestEvent::CloseSpan { id: 2, tsc: 3 }, &[1, 2, 3]),
            (
                GuestEvent::LogEvent {
                    parent_id: 2,
                    name: Cow::Borrowed("n"),
                    tsc: 3,
                    fields: vec![],
                },
                &[2, 2, 1, b'n', 3, 0],
            ),
            (
                GuestEvent::EditSpan { id: 2, fields: vec![field()] },
                &[3, 2, 1, 1, b'k', 1, b'v'],
            ),
            (GuestEvent::GuestStart { tsc: 1 }, &[4, 1]),
        ];

        for (event, expected) in cases {
            let encoded = postcard::to_extend(&event, Vec::new()).unwrap();
            assert_eq!(encoded, expected, "{event:?}");
        }
    }

    fn sample_events() -> Vec<GuestEvent<'static>> {
        let field = EventKeyValue {
            key: Cow::Borrowed("key"),
            value: Cow::Borrowed("value"),
        };
        vec![
            GuestEvent::GuestStart { tsc: 1 },
            GuestEvent::OpenSpan {
                id: 2,
                parent_id: Some(1),
                name: Cow::Borrowed("span"),
                target: Cow::Borrowed("target"),
                tsc: 3,
                fields: vec![field.clone()],
            },
            GuestEvent::LogEvent {
                parent_id: 2,
                name: Cow::Borrowed("log"),
                tsc: 4,
                fields: vec![field.clone()],
            },
            GuestEvent::EditSpan {
                id: 2,
                fields: vec![field],
            },
            GuestEvent::CloseSpan { id: 2, tsc: 5 },
        ]
    }

    #[test]
    fn batch_round_trips_and_borrows() {
        let events = sample_events();
        let mut encoder =
            EventsBatchEncoderGeneric::new(MAX_TRACE_DATA_SIZE, |_| panic!("batch should fit"));
        for event in &events {
            encoder.encode(event);
        }

        let batch = encoder.finish();
        let decoded = EventsBatchDecoder.decode(batch).unwrap();
        assert_eq!(decoded, events);

        let GuestEvent::OpenSpan { name, .. } = &decoded[1] else {
            panic!("expected OpenSpan");
        };
        assert!(matches!(name, Cow::Borrowed(_)));
    }

    #[test]
    fn full_batches_are_reported() {
        let reported = RefCell::new(Vec::new());
        let mut encoder = EventsBatchEncoderGeneric::new(32, |batch: &[u8]| {
            reported.borrow_mut().push(batch.to_vec())
        });
        for event in sample_events() {
            encoder.encode(&event);
        }
        encoder.flush();

        let decoded: Vec<_> = reported
            .borrow()
            .iter()
            .flat_map(|batch| {
                EventsBatchDecoder
                    .decode(batch)
                    .unwrap()
                    .into_iter()
                    .map(|event| format!("{event:?}"))
                    .collect::<Vec<_>>()
            })
            .collect();
        let expected: Vec<_> = sample_events()
            .iter()
            .map(|event| format!("{event:?}"))
            .collect();
        assert_eq!(decoded, expected);
        assert!(reported.borrow().len() > 1);
    }

    #[test]
    fn decoder_rejects_truncated_and_trailing_data() {
        let mut encoder = EventsBatchEncoderGeneric::new(MAX_TRACE_DATA_SIZE, |_| {});
        encoder.encode(&GuestEvent::GuestStart { tsc: 7 });
        let batch = encoder.finish().to_vec();

        assert!(
            EventsBatchDecoder
                .decode(&batch[..batch.len() - 1])
                .is_err()
        );

        let mut trailing = batch.clone();
        trailing.push(0);
        assert!(EventsBatchDecoder.decode(&trailing).is_err());
    }
}
