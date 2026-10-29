// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 The Hyperlight Authors.
extern crate alloc;

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Debug;

use hyperlight_common::flatbuffer_wrappers::guest_trace_data::EventKeyValue;
use tracing_core::field::{Field, Visit};

/// Visitor implementation to collect fields into a vector of key-value pairs
pub(crate) struct FieldsVisitor<'a> {
    pub out: &'a mut Vec<EventKeyValue<'static>>,
}

impl FieldsVisitor<'_> {
    fn push(&mut self, f: &Field, value: String) {
        self.out.push(EventKeyValue {
            key: Cow::Borrowed(f.name()),
            value: Cow::Owned(value),
        });
    }
}

impl<'a> Visit for FieldsVisitor<'a> {
    /// Record a byte slice field
    /// # Arguments
    /// * `field` - The field metadata
    /// * `value` - The byte slice value
    fn record_bytes(&mut self, f: &Field, v: &[u8]) {
        self.push(f, alloc::format!("{v:?}"));
    }

    /// Record a string field
    /// # Arguments
    /// * `f` - The field metadata
    /// * `v` - The string value
    fn record_str(&mut self, f: &Field, v: &str) {
        self.push(f, String::from(v));
    }

    /// Record a debug field
    /// # Arguments
    /// * `f` - The field metadata
    /// * `v` - The debug value
    fn record_debug(&mut self, f: &Field, v: &dyn Debug) {
        self.push(f, alloc::format!("{v:?}"));
    }
}
