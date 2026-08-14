// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! Host virtqueue construction and canonical snapshot validation.
//!
//! Runtime consumers bind bounded ring views to the host-owned fixed transport
//! arena and payload views to dynamic scratch. They start at cursor zero before
//! the first guest entry and observe descriptors published by the guest later.
//!
//! H2G requests are written into guest-prefilled chains. G2H codec helpers copy
//! untrusted guest requests and results into host-owned values before use.
//! Shared wire framing lives in `hyperlight_common::transport`.
//!
//! Snapshot capture and restore validate canonical ring images against the
//! configured arena before exposing consumers.

mod codec;
mod mem;
#[cfg(test)]
pub(crate) mod tests;

use core::ops::Range;

pub(crate) use codec::{
    get_host_function_call, read_guest_function_call_result, read_guest_log_data,
    read_message_header, try_write_response,
};
use hyperlight_common::layout::{QueueDims, TransportArena};
use hyperlight_common::virtq::canonical::{validate_canon_image, validate_canon_prefix};
use hyperlight_common::virtq::{
    Layout as VirtqLayout, MemOps, Notifier, QueueStats, VirtqConsumer,
};
use mem::{HostMemOps, ImageMem};

use super::layout::{BaseGpaRegion, SandboxMemoryLayout};
use super::shared_mem::{HostSharedMemory, SharedMemory};
use crate::{Result, new_error};

/// Host-side G2H virtqueue consumer.
pub(crate) type G2hConsumer = VirtqConsumer<HostMemOps, HostNotifier>;

/// Host-side H2G virtqueue consumer.
pub(crate) type H2gConsumer = VirtqConsumer<HostMemOps, HostNotifier>;

/// No-op notifier because the host completes work during the current VM exit.
#[derive(Clone, Copy)]
pub(crate) struct HostNotifier;

impl Notifier for HostNotifier {
    fn notify(&self, _stats: QueueStats) {}
}

/// Create both host consumers before the first guest entry.
///
/// Ring contents are not inspected because the guest has not initialized them
/// yet. Consumer cursors start at zero and observe descriptors published later.
pub(crate) fn create_consumers(
    layout: &SandboxMemoryLayout,
    scratch_mem: &HostSharedMemory,
) -> Result<(G2hConsumer, H2gConsumer)> {
    let validator = Validator::new(layout)?;
    let regions = validator.resolve_gva_regions()?;
    let g2h_layout = validator.config.g2h.layout(&regions.g2h_ring)?;
    let h2g_layout = validator.config.h2g.layout(&regions.h2g_ring)?;

    build_consumers(scratch_mem, regions, g2h_layout, h2g_layout)
}

/// Bind consumers to separately bounded ring and payload mappings.
fn build_consumers(
    scratch_mem: &HostSharedMemory,
    regions: GvaRegions,
    g2h_layout: VirtqLayout,
    h2g_layout: VirtqLayout,
) -> Result<(G2hConsumer, H2gConsumer)> {
    let g2h_ring_mem = HostMemOps::new(scratch_mem, regions.g2h_ring)?;
    let g2h_pool_mem = HostMemOps::new(scratch_mem, regions.payload.clone())?;
    let h2g_ring_mem = HostMemOps::new(scratch_mem, regions.h2g_ring)?;
    let h2g_pool_mem = HostMemOps::new(scratch_mem, regions.payload)?;

    Ok((
        VirtqConsumer::new_split(g2h_layout, g2h_ring_mem, g2h_pool_mem, HostNotifier),
        VirtqConsumer::new_split(h2g_layout, h2g_ring_mem, h2g_pool_mem, HostNotifier),
    ))
}

/// Capture the canonical ring state omitted from the main memory snapshot.
///
/// Retained pool pages are captured through ordinary stable guest mappings.
pub(crate) fn snapshot(
    layout: &SandboxMemoryLayout,
    scratch_mem: &HostSharedMemory,
) -> Result<VirtqSnapshot> {
    let validator = Validator::new(layout)?;

    let arena_gpa = read_published_arena_gpa(scratch_mem)?;
    let regions = validator.validate_published_arena(arena_gpa)?;

    let g2h_mem = HostMemOps::new(scratch_mem, regions.g2h_ring.clone())?;
    validator.validate_g2h(&g2h_mem, regions.g2h_ring.clone())?;

    let h2g_mem = HostMemOps::new(scratch_mem, regions.h2g_ring.clone())?;
    validator.validate_h2g(&h2g_mem, regions.h2g_ring.clone(), regions.payload.clone())?;

    // The vCPU is stopped, so the ring images and snapshotted guest producer
    // bookkeeping describe the same instant.
    Ok(VirtqSnapshot {
        scratch_size: layout.get_scratch_size(),
        g2h_ring: read_ring(scratch_mem, regions.g2h_ring)?,
        h2g_ring: read_ring(scratch_mem, regions.h2g_ring)?,
    })
}

/// Validate and restore one canonical transport image.
///
/// Validation completes before restored scratch is mutated. Fresh consumers
/// start from the canonical cursor state encoded in the rings.
pub(crate) fn restore(
    layout: &SandboxMemoryLayout,
    scratch_mem: &HostSharedMemory,
    snapshot: &VirtqSnapshot,
) -> Result<(G2hConsumer, H2gConsumer)> {
    let regions = Validator::new(layout)?.validate_snapshot(snapshot)?;

    write_published_arena_gpa(scratch_mem, layout.get_transport_arena().base_addr())?;
    write_ring(scratch_mem, regions.g2h_ring, &snapshot.g2h_ring)?;
    write_ring(scratch_mem, regions.h2g_ring, &snapshot.h2g_ring)?;
    create_consumers(layout, scratch_mem)
}

/// Bounded GVA regions derived from validated transport GPAs.
struct GvaRegions {
    /// Guest-to-host packed ring image.
    g2h_ring: Range<u64>,
    /// Host-to-guest packed ring image.
    h2g_ring: Range<u64>,
    /// Dynamic scratch range available for descriptor buffers.
    payload: Range<u64>,
}

#[derive(Clone, Copy)]
struct QueueConfig {
    /// Address-independent queue dimensions.
    dims: QueueDims,
    /// Size of each buffer in the pool in bytes.
    buffer_size: usize,
}

impl QueueConfig {
    fn new(dims: QueueDims, buffer_size: usize) -> Result<Self> {
        if buffer_size == 0 {
            return Err(new_error!("buffer size is zero"));
        }

        Ok(Self { dims, buffer_size })
    }

    fn layout(&self, ring: &Range<u64>) -> Result<VirtqLayout> {
        // SAFETY: `ring` is derived from the validated fixed transport arena.
        unsafe { VirtqLayout::from_base(ring.start, self.dims.size()) }
            .map_err(|error| new_error!("invalid ring layout: {error}"))
    }
}

/// Host-owned transport dimensions.
#[derive(Clone, Copy)]
struct Config {
    /// Host-requested G2H configuration.
    g2h: QueueConfig,
    /// Host-requested H2G configuration.
    h2g: QueueConfig,
    /// Fixed host-assigned transport arena.
    arena: TransportArena,
    /// Number of canonical single-buffer H2G receive chains.
    h2g_prefill_descs: usize,
}

impl Config {
    /// Compute the host transport configuration from the memory layout.
    fn from_layout(layout: &SandboxMemoryLayout) -> Result<Self> {
        let g2h = QueueConfig::new(layout.get_g2h_queue_dims(), layout.get_g2h_buffer_size())?;
        let h2g = QueueConfig::new(layout.get_h2g_queue_dims(), layout.get_h2g_buffer_size())?;

        let h2g_prefill_descs =
            usize::from(h2g.dims.size().get()).min(h2g.dims.pool_len() / h2g.buffer_size);

        let arena = layout.get_transport_arena();

        Ok(Self {
            g2h,
            h2g,
            arena,
            h2g_prefill_descs,
        })
    }
}

/// Canonical in-memory transport state excluded from ordinary snapshot pages.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct VirtqSnapshot {
    /// Scratch size used to derive transport GVAs.
    scratch_size: usize,
    /// Canonical guest-to-host ring image.
    g2h_ring: Vec<u8>,
    /// Canonical host-to-guest ring image.
    h2g_ring: Vec<u8>,
}

impl VirtqSnapshot {
    pub(crate) fn new(scratch_size: usize, g2h_ring: Vec<u8>, h2g_ring: Vec<u8>) -> Self {
        Self {
            scratch_size,
            g2h_ring,
            h2g_ring,
        }
    }

    pub(crate) fn scratch_size(&self) -> usize {
        self.scratch_size
    }

    pub(crate) fn g2h_ring(&self) -> &[u8] {
        &self.g2h_ring
    }

    pub(crate) fn h2g_ring(&self) -> &[u8] {
        &self.h2g_ring
    }

    /// Validate every captured field before mutating restored scratch.
    pub(crate) fn preflight(&self, layout: &SandboxMemoryLayout) -> Result<()> {
        Validator::new(layout)?.validate_snapshot(self).map(|_| ())
    }
}

/// Validates live and captured transport images against one host layout.
struct Validator<'a> {
    config: Config,
    layout: &'a SandboxMemoryLayout,
}

impl<'a> Validator<'a> {
    fn new(layout: &'a SandboxMemoryLayout) -> Result<Self> {
        Ok(Self {
            config: Config::from_layout(layout)?,
            layout,
        })
    }

    /// Validate a canonical G2H image and return its layout.
    fn validate_g2h<M: MemOps>(&self, mem: &M, ring: Range<u64>) -> Result<VirtqLayout> {
        let layout = self.config.g2h.layout(&ring)?;

        validate_canon_image(mem, layout, 0, |_, _| false)
            .map_err(|error| new_error!("invalid canonical G2H image: {error}"))?;

        Ok(layout)
    }

    /// Validate a canonical H2G image and return its layout.
    ///
    /// Each available chain contains one writable descriptor. Descriptors must
    /// name distinct ranges inside dynamic scratch.
    fn validate_h2g<M: MemOps>(
        &self,
        mem: &M,
        ring: Range<u64>,
        payload: Range<u64>,
    ) -> Result<VirtqLayout> {
        let layout = self.config.h2g.layout(&ring)?;
        let bufsz = self.config.h2g.buffer_size;
        let prefill = self.config.h2g_prefill_descs;

        if prefill == 0 {
            return Err(new_error!("H2G pool has no complete buffers"));
        }

        // Record the accepted descriptor ranges to detect overlaps.
        let mut accepted: Vec<Range<u64>> = Vec::with_capacity(prefill);

        let image = validate_canon_prefix(mem, layout, prefill, |_, elem| {
            // all descriptors must be writable and match the configured buffer size
            if !elem.writable || usize::try_from(elem.len).ok() != Some(bufsz) {
                return false;
            }

            let Some(end) = elem.addr.checked_add(u64::from(elem.len)) else {
                return false;
            };

            // all descriptors must remain inside dynamic scratch
            if elem.addr < payload.start || end > payload.end {
                return false;
            }

            let buf = elem.addr..end;

            // all descriptors must name distinct ranges
            if accepted
                .iter()
                .any(|other| buf.start < other.end && other.start < buf.end)
            {
                return false;
            }

            accepted.push(buf);
            true
        })
        .map_err(|error| new_error!("invalid canonical H2G image: {error}"))?;

        if image.is_empty() || image.iter().any(|chain| chain.buffers().len() != 1) {
            return Err(new_error!("invalid initial H2G receive buffers"));
        }

        Ok(layout)
    }

    /// Validate the published arena and return its GVA regions.
    fn validate_published_arena(&self, arena_gpa: u64) -> Result<GvaRegions> {
        if arena_gpa != self.config.arena.base_addr() {
            return Err(new_error!("published transport arena is invalid"));
        }

        self.resolve_gva_regions()
    }

    fn validate_snapshot(&self, snapshot: &VirtqSnapshot) -> Result<GvaRegions> {
        if snapshot.scratch_size != self.layout.get_scratch_size() {
            return Err(new_error!(
                "virtqueue snapshot scratch size {} does not match layout size {}",
                snapshot.scratch_size,
                self.layout.get_scratch_size()
            ));
        }

        let regions = self.resolve_gva_regions()?;
        validate_ring_len("G2H", &snapshot.g2h_ring, self.config.g2h.dims.ring_len())?;
        validate_ring_len("H2G", &snapshot.h2g_ring, self.config.h2g.dims.ring_len())?;

        let g2h_mem = ImageMem::new(regions.g2h_ring.start, &snapshot.g2h_ring);
        self.validate_g2h(&g2h_mem, regions.g2h_ring.clone())?;

        let h2g_mem = ImageMem::new(regions.h2g_ring.start, &snapshot.h2g_ring);
        self.validate_h2g(
            &h2g_mem,
            regions.h2g_ring.clone(),
            self.snapshot_payload_region()?,
        )?;

        Ok(regions)
    }

    fn snapshot_payload_region(&self) -> Result<Range<u64>> {
        let scratch_base_gpa =
            hyperlight_common::layout::scratch_base_gpa(self.layout.get_scratch_size());
        let scratch_base_gva =
            hyperlight_common::layout::scratch_base_gva(self.layout.get_scratch_size());
        let to_gva = |gpa: u64| {
            gpa.checked_sub(scratch_base_gpa)
                .and_then(|offset| scratch_base_gva.checked_add(offset))
                .ok_or_else(|| new_error!("scratch GPA {gpa:#x} to GVA translation overflow"))
        };

        Ok(to_gva(self.config.arena.end_addr())?
            ..to_gva(hyperlight_common::layout::scratch_allocator_limit_gpa())?)
    }

    /// Translate validated transport GPAs into the GVA ranges used by descriptors.
    fn resolve_gva_regions(&self) -> Result<GvaRegions> {
        let to_gva = |gpa| {
            let resolved = self
                .layout
                .resolve_gpa(gpa, &[])
                .ok_or_else(|| new_error!("GPA {gpa:#x} is outside scratch"))?;

            if !matches!(resolved.base, BaseGpaRegion::Scratch(())) {
                return Err(new_error!("GPA {gpa:#x} is outside scratch"));
            }

            hyperlight_common::layout::scratch_base_gva(self.layout.get_scratch_size())
                .checked_add(u64::try_from(resolved.offset)?)
                .ok_or_else(|| new_error!("GPA {gpa:#x} to GVA translation overflow"))
        };

        let (g2h_ring_addr, h2g_ring_addr, g2h_ring_len, h2g_ring_len) = (
            self.config.arena.g2h_ring_addr(),
            self.config.arena.h2g_ring_addr(),
            self.config.g2h.dims.ring_len(),
            self.config.h2g.dims.ring_len(),
        );

        let scratch_base_gpa =
            hyperlight_common::layout::scratch_base_gpa(self.layout.get_scratch_size());
        let scratch_base_gva =
            hyperlight_common::layout::scratch_base_gva(self.layout.get_scratch_size());
        let to_scratch_gva = |gpa: u64| {
            gpa.checked_sub(scratch_base_gpa)
                .and_then(|offset| scratch_base_gva.checked_add(offset))
                .ok_or_else(|| new_error!("scratch GPA {gpa:#x} to GVA translation overflow"))
        };

        let payload = to_scratch_gva(self.layout.get_first_free_scratch_gpa())?
            ..to_scratch_gva(hyperlight_common::layout::scratch_allocator_limit_gpa())?;

        Ok(GvaRegions {
            g2h_ring: checked_region(to_gva(g2h_ring_addr)?, g2h_ring_len, "G2H ring")?,
            h2g_ring: checked_region(to_gva(h2g_ring_addr)?, h2g_ring_len, "H2G ring")?,
            payload,
        })
    }
}

/// Read the transport arena GPA from scratch-top metadata.
fn read_published_arena_gpa(scratch_mem: &HostSharedMemory) -> Result<u64> {
    let offset = hyperlight_common::layout::SCRATCH_TOP_TRANSPORT_ARENA_GPA_OFFSET as usize;
    Ok(scratch_mem.read::<u64>(scratch_mem.mem_size() - offset)?)
}

/// Publish the fixed transport arena GPA in scratch-top metadata.
fn write_published_arena_gpa(scratch_mem: &HostSharedMemory, arena_gpa: u64) -> Result<()> {
    let offset = hyperlight_common::layout::SCRATCH_TOP_TRANSPORT_ARENA_GPA_OFFSET as usize;
    Ok(scratch_mem.write::<u64>(scratch_mem.mem_size() - offset, arena_gpa)?)
}

/// Copy one ring image from its bounded scratch mapping.
fn read_ring(scratch_mem: &HostSharedMemory, ring: Range<u64>) -> Result<Vec<u8>> {
    let len = usize::try_from(
        ring.end
            .checked_sub(ring.start)
            .ok_or_else(|| new_error!("invalid ring range"))?,
    )?;

    let mem = HostMemOps::new(scratch_mem, ring.clone())?;
    let mut bytes = vec![0; len];
    mem.read(ring.start, &mut bytes)?;

    Ok(bytes)
}

/// Copy one validated ring image into its bounded scratch mapping.
fn write_ring(scratch_mem: &HostSharedMemory, ring: Range<u64>, bytes: &[u8]) -> Result<()> {
    validate_ring_len("restored", bytes, usize::try_from(ring.end - ring.start)?)?;
    let mem = HostMemOps::new(scratch_mem, ring.clone())?;
    mem.write(ring.start, bytes)
}

/// Require a captured ring image to match its configured region exactly.
fn validate_ring_len(direction: &str, bytes: &[u8], expected: usize) -> Result<()> {
    if bytes.len() != expected {
        return Err(new_error!(
            "{direction} snapshot ring length {} and expected length {expected}",
            bytes.len()
        ));
    }
    Ok(())
}

/// Build a GVA range while checking address arithmetic.
fn checked_region(start: u64, len: usize, tag: &str) -> Result<Range<u64>> {
    let end = start
        .checked_add(u64::try_from(len)?)
        .ok_or_else(|| new_error!("{tag} GVA range overflow"))?;

    Ok(start..end)
}
