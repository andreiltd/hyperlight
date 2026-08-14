// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! Guest-side [`MemOps`] implementation for virtqueue access.
//!
//! Virtqueue descriptors use transient scratch GVAs. Retained buffer owners
//! use separately allocated aliases so snapshots preserve their mappings.

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::mem::{align_of, size_of};
use core::ops::Range;
use core::sync::atomic::{AtomicU16, Ordering};

use hyperlight_common::layout as common_layout;
use hyperlight_common::virtq::MemOps;
use hyperlight_common::vmem::PAGE_SIZE;
use vm_allocator::{AddressAllocator, AllocPolicy, RangeInclusive as VmRange};

use crate::layout;

/// Guest page-table operations used for stable buffer mappings.
#[derive(Clone, Copy, Debug)]
pub struct Mapper {
    /// Maps a page-aligned GPA range at a page-aligned alias GVA.
    map_fn: unsafe fn(u64, u64, u64),
    /// Removes a page-aligned alias GVA range.
    unmap_fn: unsafe fn(u64, u64),
}

impl Mapper {
    /// Creates buffer mapping operations.
    ///
    /// The callbacks receive page-aligned ranges. `map` must make the mapping
    /// visible before returning. `unmap` must invalidate stale translations.
    pub const fn new(map: unsafe fn(u64, u64, u64), unmap: unsafe fn(u64, u64)) -> Self {
        Self {
            map_fn: map,
            unmap_fn: unmap,
        }
    }

    /// Maps a stable owner alias.
    ///
    /// # Safety
    ///
    /// Both addresses and `len` must be page-aligned. The GPA range must be
    /// mapped scratch memory and the alias GVA range must be reserved.
    unsafe fn map(&self, gpa: u64, gva: u64, len: u64) {
        unsafe { (self.map_fn)(gpa, gva, len) };
    }

    /// Removes a stable owner alias.
    ///
    /// # Safety
    ///
    /// The complete range must identify a live mapping created by [`Self::map`].
    unsafe fn unmap(&self, gva: u64, len: u64) {
        unsafe { (self.unmap_fn)(gva, len) };
    }

    /// Creates mapping operations that perform no page-table changes.
    #[cfg(test)]
    const fn noop() -> Self {
        unsafe fn map(_gpa: u64, _gva: u64, _len: u64) {}
        unsafe fn unmap(_gva: u64, _len: u64) {}
        Self::new(map, unmap)
    }
}

/// Page-aligned span covering one byte range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PageSpan {
    /// Page-aligned start address.
    base: u64,
    /// Exclusive page-aligned end address.
    end: u64,
    /// Byte-range offset from `base`.
    offset: u64,
}

impl PageSpan {
    /// Covers `[addr, addr + len)` with complete pages.
    fn new(addr: u64, len: usize) -> Result<Self, GuestMemError> {
        let page_mask = PAGE_SIZE as u64 - 1;
        let base = addr & !page_mask;
        let offset = addr - base;

        let len = offset
            .checked_add(len as u64)
            .and_then(|len| len.checked_next_multiple_of(PAGE_SIZE as u64))
            .ok_or(GuestMemError)?;

        let end = base.checked_add(len).ok_or(GuestMemError)?;
        Ok(Self { base, end, offset })
    }

    /// Returns the complete page range.
    fn range(self) -> Range<u64> {
        self.base..self.end
    }

    /// Returns the page-rounded span length.
    fn len(self) -> u64 {
        self.end - self.base
    }
}

/// Allocates non-overlapping ranges from the reserved virtqueue alias region.
#[derive(Debug)]
struct AliasAllocator {
    /// Reserved alias address space.
    allocator: AddressAllocator,
    /// Ranges with active page-table mappings.
    live: Vec<Range<u64>>,
}

impl AliasAllocator {
    /// Creates an allocator spanning the architecture's alias GVA region.
    fn new() -> Self {
        let base = common_layout::VIRTQ_BUFFER_GVA_MIN as u64;
        let size = common_layout::VIRTQ_BUFFER_GVA_MAX as u64 - base + 1;

        // TODO: vm-allocator is likely overkill for one page-aligned
        // region. Replace it with a small local range allocator.
        let allocator = AddressAllocator::new(base, size).expect("valid virtqueue alias range");

        Self {
            allocator,
            live: Vec::new(),
        }
    }

    /// Allocates and returns a range of `len` bytes.
    ///
    /// Callers provide a nonzero, page-rounded length.
    fn allocate(&mut self, len: u64) -> Result<Range<u64>, GuestMemError> {
        if len == 0 || !len.is_multiple_of(PAGE_SIZE as u64) {
            return Err(GuestMemError);
        }

        self.live.try_reserve(1).map_err(|_| GuestMemError)?;

        let allocated = self
            .allocator
            .allocate(len, PAGE_SIZE as u64, AllocPolicy::FirstMatch)
            .map_err(|_| GuestMemError)?;

        let range = allocated.start()..allocated.end().checked_add(1).ok_or(GuestMemError)?;
        self.live.push(range.clone());

        Ok(range)
    }

    /// Releases one exact live range.
    fn release(&mut self, range: Range<u64>) -> Result<(), GuestMemError> {
        let index = self
            .live
            .iter()
            .position(|live| live == &range)
            .ok_or(GuestMemError)?;

        let end = range.end.checked_sub(1).ok_or(GuestMemError)?;
        let allocated = VmRange::new(range.start, end).map_err(|_| GuestMemError)?;

        self.allocator.free(&allocated).map_err(|_| GuestMemError)?;
        self.live.swap_remove(index);

        Ok(())
    }

    /// Returns whether `range` lies within one live alias.
    fn contains(&self, range: &Range<u64>) -> bool {
        self.live.iter().any(|live| contains_range(live, range))
    }

    /// Returns whether `range` identifies one complete live alias.
    fn contains_exact(&self, range: &Range<u64>) -> bool {
        self.live.iter().any(|live| live == range)
    }
}

/// Stable mappings for retained virtqueue buffers.
#[derive(Debug)]
struct BufferMappings {
    /// Alias address-space allocator.
    aliases: AliasAllocator,
    /// Guest page-table operations.
    mapper: Mapper,
    /// GPA corresponding to `scratch.start`.
    scratch_gpa: u64,
    /// Complete scratch GVA range.
    scratch: Range<u64>,
}

impl BufferMappings {
    /// Creates an empty owner mapping set.
    fn new(mapper: Mapper, scratch_gpa: u64, scratch: Range<u64>) -> Self {
        Self {
            aliases: AliasAllocator::new(),
            mapper,
            scratch_gpa,
            scratch,
        }
    }

    /// Returns whether `range` lies within one live owner alias.
    fn contains(&self, range: &Range<u64>) -> bool {
        self.aliases.contains(range)
    }

    /// Maps an initialized allocation at a stable owner address.
    fn map_buf(&mut self, addr: u64, len: usize, capacity: usize) -> Result<u64, GuestMemError> {
        if len > capacity {
            return Err(GuestMemError);
        }

        let allocation = checked_range(addr, capacity)?;
        if !contains_range(&self.scratch, &allocation) {
            return Err(GuestMemError);
        }

        if len == 0 {
            return Ok(addr);
        }

        let source = PageSpan::new(addr, len)?;
        let initialized_end = checked_range(addr, len)?.end;
        let tail_end = source.end.min(allocation.end);
        let tail_len = usize::try_from(tail_end - initialized_end).map_err(|_| GuestMemError)?;

        if tail_len != 0 {
            // SAFETY: The producer owns the allocation until its BufferOwner
            // is constructed. Only bytes visible through the alias are cleared.
            unsafe { (initialized_end as *mut u8).write_bytes(0, tail_len) };
        }

        let scratch_offset = source
            .base
            .checked_sub(self.scratch.start)
            .ok_or(GuestMemError)?;

        let gpa = self
            .scratch_gpa
            .checked_add(scratch_offset)
            .ok_or(GuestMemError)?;

        let alias = self.aliases.allocate(source.len())?;
        let Some(owner_addr) = alias.start.checked_add(source.offset) else {
            self.aliases.release(alias)?;
            return Err(GuestMemError);
        };

        // SAFETY: Both addresses and the length are page-aligned. The source
        // GPA is mapped scratch memory and `alias` is exclusively reserved.
        unsafe { self.mapper.map(gpa, alias.start, source.len()) };
        Ok(owner_addr)
    }

    /// Unmaps and releases a stable owner address.
    fn unmap_buf(&mut self, addr: u64, len: usize) -> Result<(), GuestMemError> {
        if len == 0 {
            return Ok(());
        }

        let owner = PageSpan::new(addr, len)?;
        let alias = owner.range();
        if !self.aliases.contains_exact(&alias) {
            return Err(GuestMemError);
        }

        // SAFETY: Exact live-range validation proves that this object owns the
        // complete page-aligned alias mapping.
        unsafe { self.mapper.unmap(alias.start, owner.len()) };
        self.aliases.release(alias)
    }
}

/// Guest-side memory accessor for GVA-valued virtqueue addresses.
#[derive(Clone, Debug)]
pub struct GuestMemOps {
    /// Complete scratch GVA range.
    scratch: Range<u64>,
    /// Stable owner mappings shared by all clones of this accessor.
    mappings: Rc<RefCell<BufferMappings>>,
}

// SAFETY: Hyperlight guests have one vCPU and serialize guest entry.
unsafe impl Send for GuestMemOps {}

/// Invalid, overflowing, or exhausted guest virtqueue memory operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestMemError;

impl GuestMemOps {
    /// Creates an accessor for the initialized guest scratch mapping.
    pub(super) fn for_scratch(mapper: Mapper) -> Self {
        let scratch_len = unsafe { layout::scratch_size_gva().read_volatile() };
        let scratch_gva = layout::scratch_base_gva();

        let scratch_end = scratch_gva
            .checked_add(scratch_len)
            .expect("scratch end overflow");

        let scratch = scratch_gva..scratch_end;

        let mappings = Rc::new(RefCell::new(BufferMappings::new(
            mapper,
            layout::scratch_base_gpa(),
            scratch.clone(),
        )));

        Self { scratch, mappings }
    }

    /// Validates a range and returns its guest pointer.
    fn ptr(&self, addr: u64, len: usize) -> Result<*mut u8, GuestMemError> {
        let range = checked_range(addr, len)?;
        if contains_range(&self.scratch, &range) {
            return Ok(addr as *mut u8);
        }

        self.mappings
            .borrow()
            .contains(&range)
            .then_some(addr as *mut u8)
            .ok_or(GuestMemError)
    }

    /// Validates an aligned ring field and returns its atomic reference.
    fn atomic(&self, addr: u64) -> Result<&AtomicU16, GuestMemError> {
        let ptr = self.ptr(addr, size_of::<AtomicU16>())?;
        if !(ptr as usize).is_multiple_of(align_of::<AtomicU16>()) {
            return Err(GuestMemError);
        }
        // SAFETY: Ring atomics remain inside the live scratch mapping.
        Ok(unsafe { &*ptr.cast::<AtomicU16>() })
    }
}

// SAFETY: Every address is restricted to scratch or a tracked stable alias.
// Payload references rely on descriptor ownership, and ring flags use aligned
// atomics in scratch.
unsafe impl MemOps for GuestMemOps {
    type Error = GuestMemError;

    fn read(&self, addr: u64, dst: &mut [u8]) -> Result<(), Self::Error> {
        let src = self.ptr(addr, dst.len())?;
        // SAFETY: `src` covers `dst.len()` initialized scratch bytes.
        unsafe { src.copy_to_nonoverlapping(dst.as_mut_ptr(), dst.len()) };
        Ok(())
    }

    fn write(&self, addr: u64, src: &[u8]) -> Result<(), Self::Error> {
        let dst = self.ptr(addr, src.len())?;
        // SAFETY: `dst` covers `src.len()` scratch bytes.
        unsafe { src.as_ptr().copy_to_nonoverlapping(dst, src.len()) };
        Ok(())
    }

    fn load_acquire(&self, addr: u64) -> Result<u16, Self::Error> {
        Ok(self.atomic(addr)?.load(Ordering::Acquire))
    }

    fn store_release(&self, addr: u64, val: u16) -> Result<(), Self::Error> {
        self.atomic(addr)?.store(val, Ordering::Release);
        Ok(())
    }

    unsafe fn as_slice(&self, addr: u64, len: usize) -> Result<&[u8], Self::Error> {
        let ptr = self.ptr(addr, len)?;
        // SAFETY: The caller upholds descriptor ownership for this range.
        Ok(unsafe { core::slice::from_raw_parts(ptr, len) })
    }

    #[allow(clippy::mut_from_ref)]
    unsafe fn as_mut_slice(&self, addr: u64, len: usize) -> Result<&mut [u8], Self::Error> {
        let ptr = self.ptr(addr, len)?;
        // SAFETY: The caller upholds exclusive descriptor ownership.
        Ok(unsafe { core::slice::from_raw_parts_mut(ptr, len) })
    }

    fn map_buf(&self, addr: u64, len: usize, capacity: usize) -> Result<u64, Self::Error> {
        self.mappings.borrow_mut().map_buf(addr, len, capacity)
    }

    fn unmap_buf(&self, addr: u64, len: usize) -> Result<(), Self::Error> {
        self.mappings.borrow_mut().unmap_buf(addr, len)
    }
}

/// Creates a checked half-open byte range.
fn checked_range(addr: u64, len: usize) -> Result<Range<u64>, GuestMemError> {
    let len = u64::try_from(len).map_err(|_| GuestMemError)?;
    let end = addr.checked_add(len).ok_or(GuestMemError)?;
    Ok(addr..end)
}

/// Returns whether `outer` contains the complete `inner` range.
fn contains_range(outer: &Range<u64>, inner: &Range<u64>) -> bool {
    inner.start >= outer.start && inner.end <= outer.end
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use hyperlight_common::virtq::MemOps;

    use super::*;

    #[test]
    fn guest_mem_access_is_bounded_by_scratch() {
        const LEN: usize = 0x4000;
        let mut backing = vec![0u8; LEN + PAGE_SIZE];
        let base = (backing.as_mut_ptr() as usize).next_multiple_of(PAGE_SIZE) as u64;
        let scratch = base..base + LEN as u64;
        let mappings = Rc::new(RefCell::new(BufferMappings::new(
            Mapper::noop(),
            0,
            scratch.clone(),
        )));

        let mem = GuestMemOps {
            scratch: scratch.clone(),
            mappings,
        };

        mem.write(base, &[1, 2, 3, 4]).unwrap();
        let mut bytes = [0; 4];
        mem.read(base, &mut bytes).unwrap();
        assert_eq!(bytes, [1, 2, 3, 4]);

        mem.store_release(base, 0x1234).unwrap();
        assert_eq!(mem.load_acquire(base).unwrap(), 0x1234);
        let owner = mem.map_buf(base, 2, 2).unwrap();
        assert_ne!(owner, base);
        mem.unmap_buf(owner, 2).unwrap();

        assert!(mem.write(base + LEN as u64 - 1, &[1, 2]).is_err());
        assert!(mem.load_acquire(base + 1).is_err());
    }

    #[test]
    fn page_span_covers_unaligned_cross_page_range() {
        assert!(PageSpan::new(u64::MAX, 1).is_err());

        let addr = 3 * PAGE_SIZE as u64 - 17;
        let span = PageSpan::new(addr, 34).unwrap();

        assert_eq!(
            span,
            PageSpan {
                base: 2 * PAGE_SIZE as u64,
                end: 4 * PAGE_SIZE as u64,
                offset: PAGE_SIZE as u64 - 17,
            }
        );
    }

    #[test]
    fn alias_release_reuses_space() {
        let mut aliases = AliasAllocator::new();
        assert!(aliases.allocate(0).is_err());
        assert!(aliases.allocate(PAGE_SIZE as u64 - 1).is_err());

        let first = aliases.allocate(PAGE_SIZE as u64).unwrap();
        let second = aliases.allocate(PAGE_SIZE as u64).unwrap();

        aliases.release(first.clone()).unwrap();
        aliases.release(second.clone()).unwrap();

        assert_eq!(aliases.allocate(PAGE_SIZE as u64).unwrap(), first);
        assert_eq!(aliases.allocate(PAGE_SIZE as u64).unwrap(), second);
    }

    #[test]
    fn owner_mapping_tracks_exact_alias_range_and_zeros_tail() {
        const LEN: usize = 2 * PAGE_SIZE;
        let mut backing = vec![0xffu8; LEN + PAGE_SIZE];
        let base = (backing.as_mut_ptr() as usize).next_multiple_of(PAGE_SIZE) as u64;
        let scratch = base..base + LEN as u64;

        let mappings = Rc::new(RefCell::new(BufferMappings::new(
            Mapper::noop(),
            0,
            scratch.clone(),
        )));

        let mem = GuestMemOps {
            scratch: scratch.clone(),
            mappings,
        };
        let addr = base + 17;

        let owner = mem.map_buf(addr, 100, LEN - 17).unwrap();

        assert_eq!(owner & (PAGE_SIZE as u64 - 1), 17);
        assert!(
            unsafe { core::slice::from_raw_parts((addr + 100) as *const u8, PAGE_SIZE - 17 - 100) }
                .iter()
                .all(|byte| *byte == 0)
        );
        assert!(
            unsafe {
                core::slice::from_raw_parts((base + PAGE_SIZE as u64) as *const u8, PAGE_SIZE)
            }
            .iter()
            .all(|byte| *byte == 0xff)
        );
        assert!(mem.map_buf(owner, 100, LEN - 17).is_err());
        mem.unmap_buf(owner, 100).unwrap();
        assert!(mem.unmap_buf(owner, 100).is_err());
    }
}
