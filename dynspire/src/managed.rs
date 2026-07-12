//! DynSpire managed types, allocator vtable, and reference-counted lifecycle.
//!
//! See `docs/managed-types.md` for the full design. Summary:
//!
//! - [`DynSpireAllocator`] / [`DynSpireAllocatorVtable`] define the allocation
//!   contract. The host provides the implementation; DynSpire supplies a
//!   default backed by `std::alloc`. The allocator is configured once at
//!   `dynspire_create` and stored in the spier `State` — it is **not** passed
//!   per dispatch call.
//! - Every owned allocation is `[DynSpireHeader | payload]`. The header carries
//!   an inline refcount, a type index, a `drop_fn`, the owning allocator, and
//!   size/align. [`dynspire_retain`] / [`dynspire_release`] manage lifecycle;
//!   `release` to zero runs `drop_fn` then reclaims the block.
//! - [`DStr`], [`DSlice`], [`DString`], [`DVec`], [`DOption`] are the C-stable
//!   DynSpire types (`repr(C)`). Dynamic types carry their own allocator
//!   pointer (first field) so they are self-contained for realloc/release.

use std::alloc::Layout;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// Allocator vtable
// ---------------------------------------------------------------------------

/// Allocator vtable. DynSpire defines the contract; the host provides the
/// implementation. Every spier uses the allocator configured at
/// `dynspire_create` (stored in its `State`) for all dynamic allocations.
/// Snapshot of an allocator's current memory occupation. Returned by
/// [`DynSpireAllocator::report`] / `dynspire_allocator_report`. All counters are
/// in bytes / counts of live heap blocks (header overhead included).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DynSpireAllocatorReport {
    /// Bytes currently owned by live allocations (header + payload).
    pub live_bytes: usize,
    /// Number of live allocations.
    pub live_allocations: usize,
    /// Peak live bytes observed since the allocator was created.
    pub peak_bytes: usize,
    /// Cumulative number of allocations made.
    pub total_allocations: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DynSpireAllocatorVtable {
    pub alloc: unsafe extern "C" fn(ctx: *mut c_void, size: usize, align: usize) -> *mut u8,
    pub dealloc: unsafe extern "C" fn(ctx: *mut c_void, ptr: *mut u8, size: usize, align: usize),
    pub realloc: unsafe extern "C" fn(
        ctx: *mut c_void,
        ptr: *mut u8,
        old_size: usize,
        new_size: usize,
        align: usize,
    ) -> *mut u8,
    pub drop_allocator: unsafe extern "C" fn(ctx: *mut c_void),
    /// Returns a snapshot of the allocator's memory occupation. The default
    /// allocator has no bookkeeping and returns all zeros; the debug allocator
    /// tracks live/peak/total counters in its `ctx`.
    pub report: unsafe extern "C" fn(ctx: *mut c_void) -> DynSpireAllocatorReport,
}

/// A host-provided allocator. `vtable` is DynSpire-stable; `ctx` is opaque host
/// state. The pointer is valid in every `.so` because the allocator functions
/// live in the `dynspire` runtime crate, linked into each spier.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DynSpireAllocator {
    pub vtable: *const DynSpireAllocatorVtable,
    pub ctx: *mut c_void,
}

// A `DynSpireAllocator` is a handle to a thread-safe allocator (the vtable is a
// set of `extern "C"` functions; `ctx` is opaque). Sharing it across threads is
// sound — the allocator implementation is responsible for its own sync.
unsafe impl Send for DynSpireAllocator {}
unsafe impl Sync for DynSpireAllocator {}

// ---------------------------------------------------------------------------
// Default allocator (std::alloc)
// ---------------------------------------------------------------------------

unsafe extern "C" fn default_alloc(_ctx: *mut c_void, size: usize, align: usize) -> *mut u8 {
    if size == 0 {
        return std::ptr::null_mut();
    }
    match Layout::from_size_align(size, align) {
        Ok(layout) => std::alloc::alloc(layout),
        Err(_) => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn default_dealloc(_ctx: *mut c_void, ptr: *mut u8, size: usize, align: usize) {
    if ptr.is_null() || size == 0 {
        return;
    }
    let layout = Layout::from_size_align_unchecked(size, align);
    std::alloc::dealloc(ptr, layout);
}

unsafe extern "C" fn default_realloc(
    _ctx: *mut c_void,
    ptr: *mut u8,
    old_size: usize,
    new_size: usize,
    align: usize,
) -> *mut u8 {
    let old = Layout::from_size_align_unchecked(old_size, align);
    std::alloc::realloc(ptr, old, new_size)
}

unsafe extern "C" fn default_drop_allocator(_ctx: *mut c_void) {}

unsafe extern "C" fn default_report(
    _ctx: *mut c_void,
) -> DynSpireAllocatorReport {
    DynSpireAllocatorReport::default()
}

static DEFAULT_VTABLE: DynSpireAllocatorVtable = DynSpireAllocatorVtable {
    alloc: default_alloc,
    dealloc: default_dealloc,
    realloc: default_realloc,
    drop_allocator: default_drop_allocator,
    report: default_report,
};

/// Returns the default allocator backed by the Rust system allocator.
///
/// The returned struct points at a `static` vtable with a null `ctx`, so it is
/// safe to copy/move and to store the pointer in a spier `State`.
pub fn default_allocator() -> DynSpireAllocator {
    DynSpireAllocator {
        vtable: &DEFAULT_VTABLE as *const DynSpireAllocatorVtable,
        ctx: std::ptr::null_mut(),
    }
}

/// A process-lifetime default allocator instance. Its address is stable, so it
/// can be handed to `dynspire_create` from foreign callers (e.g. the Python
/// ctypes client) that cannot construct a `DynSpireAllocator` themselves.
static DEFAULT_ALLOCATOR: DynSpireAllocator = DynSpireAllocator {
    vtable: &DEFAULT_VTABLE as *const DynSpireAllocatorVtable,
    ctx: std::ptr::null_mut(),
};

/// C-ABI: return a pointer to the process-lifetime default allocator.
///
/// # Safety
///
/// The returned pointer is valid for the whole process lifetime; it must not be
/// freed or passed to `drop_allocator`.
#[no_mangle]
pub unsafe extern "C" fn dynspire_default_allocator() -> *mut DynSpireAllocator {
    &DEFAULT_ALLOCATOR as *const DynSpireAllocator as *mut DynSpireAllocator
}

// ---------------------------------------------------------------------------
// Debug allocator — tracks live/peak/total memory occupation for debugging
// spiers. Stats live in a process-lifetime `static`, so there is no per-instance
// bookkeeping overhead and no box to free. The performant `default_allocator`
// keeps a null `ctx` and a `report` that returns zeros, so choosing the debug
// allocator is purely opt-in.
// ---------------------------------------------------------------------------

struct DebugStats {
    live_bytes: AtomicUsize,
    live_allocations: AtomicUsize,
    peak_bytes: AtomicUsize,
    total_allocations: AtomicUsize,
}

static DEBUG_STATS: DebugStats = DebugStats {
    live_bytes: AtomicUsize::new(0),
    live_allocations: AtomicUsize::new(0),
    peak_bytes: AtomicUsize::new(0),
    total_allocations: AtomicUsize::new(0),
};

#[inline]
fn debug_record_alloc(size: usize) {
    let s = &DEBUG_STATS;
    s.live_bytes.fetch_add(size, Ordering::Relaxed);
    s.live_allocations.fetch_add(1, Ordering::Relaxed);
    s.total_allocations.fetch_add(1, Ordering::Relaxed);
    let cur = s.live_bytes.load(Ordering::Relaxed);
    let mut peak = s.peak_bytes.load(Ordering::Relaxed);
    while cur > peak {
        match s.peak_bytes.compare_exchange_weak(peak, cur, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(p) => peak = p,
        }
    }
}

#[inline]
fn debug_record_free(size: usize) {
    let s = &DEBUG_STATS;
    s.live_bytes.fetch_sub(size, Ordering::Relaxed);
    s.live_allocations.fetch_sub(1, Ordering::Relaxed);
}

unsafe extern "C" fn debug_alloc(_ctx: *mut c_void, size: usize, align: usize) -> *mut u8 {
    if size == 0 {
        return std::ptr::null_mut();
    }
    let ptr = match Layout::from_size_align(size, align) {
        Ok(layout) => std::alloc::alloc(layout),
        Err(_) => return std::ptr::null_mut(),
    };
    if !ptr.is_null() {
        debug_record_alloc(size);
    }
    ptr
}

unsafe extern "C" fn debug_dealloc(_ctx: *mut c_void, ptr: *mut u8, size: usize, align: usize) {
    if ptr.is_null() || size == 0 {
        return;
    }
    let layout = Layout::from_size_align_unchecked(size, align);
    std::alloc::dealloc(ptr, layout);
    debug_record_free(size);
}

unsafe extern "C" fn debug_realloc(
    _ctx: *mut c_void,
    ptr: *mut u8,
    old_size: usize,
    new_size: usize,
    align: usize,
) -> *mut u8 {
    let old = Layout::from_size_align_unchecked(old_size, align);
    let new = std::alloc::realloc(ptr, old, new_size);
    if new.is_null() {
        if !ptr.is_null() && old_size > 0 {
            debug_record_free(old_size);
        }
    } else {
        let s = &DEBUG_STATS;
        s.live_bytes
            .fetch_add(new_size.wrapping_sub(old_size), Ordering::Relaxed);
        let cur = s.live_bytes.load(Ordering::Relaxed);
        let mut peak = s.peak_bytes.load(Ordering::Relaxed);
        while cur > peak {
            match s.peak_bytes.compare_exchange_weak(peak, cur, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(p) => peak = p,
            }
        }
    }
    new
}

unsafe extern "C" fn debug_drop_allocator(_ctx: *mut c_void) {}

unsafe extern "C" fn debug_report(_ctx: *mut c_void) -> DynSpireAllocatorReport {
    let s = &DEBUG_STATS;
    DynSpireAllocatorReport {
        live_bytes: s.live_bytes.load(Ordering::Relaxed),
        live_allocations: s.live_allocations.load(Ordering::Relaxed),
        peak_bytes: s.peak_bytes.load(Ordering::Relaxed),
        total_allocations: s.total_allocations.load(Ordering::Relaxed),
    }
}

static DEBUG_VTABLE: DynSpireAllocatorVtable = DynSpireAllocatorVtable {
    alloc: debug_alloc,
    dealloc: debug_dealloc,
    realloc: debug_realloc,
    drop_allocator: debug_drop_allocator,
    report: debug_report,
};

/// A process-lifetime allocator that tracks memory occupation for debugging.
///
/// Unlike [`default_allocator`], every allocation/realloc/dealloc updates the
/// global debug counters, which can be read with
/// [`DynSpireAllocator::report`] / `dynspire_allocator_report`. There is one
/// shared debug instance per process, so counters aggregate across all spiers
/// created with it.
static DEBUG_ALLOCATOR: DynSpireAllocator = DynSpireAllocator {
    vtable: &DEBUG_VTABLE as *const DynSpireAllocatorVtable,
    ctx: &DEBUG_STATS as *const DebugStats as *mut c_void,
};

/// Returns the process-lifetime debug allocator (tracks memory occupation).
pub fn debug_allocator() -> DynSpireAllocator {
    DEBUG_ALLOCATOR
}

/// C-ABI: return a pointer to the process-lifetime debug allocator.
///
/// # Safety
///
/// The returned pointer is valid for the whole process lifetime; it must not be
/// freed or passed to `drop_allocator`.
#[no_mangle]
pub unsafe extern "C" fn dynspire_debug_allocator() -> *mut DynSpireAllocator {
    &DEBUG_ALLOCATOR as *const DynSpireAllocator as *mut DynSpireAllocator
}

impl DynSpireAllocator {
    /// Returns a snapshot of the allocator's current memory occupation.
    ///
    /// The default allocator has no bookkeeping and returns all zeros; the
    /// debug allocator returns live/peak/total counters tracked in its `ctx`.
    pub fn report(&self) -> DynSpireAllocatorReport {
        unsafe { ((*self.vtable).report)(self.ctx) }
    }
}

/// C-ABI: snapshot of an allocator's memory occupation.
///
/// # Safety
///
/// `alloc` must be a valid `DynSpireAllocator` (e.g. from
/// `dynspire_default_allocator` / `dynspire_debug_allocator`).
#[no_mangle]
pub unsafe extern "C" fn dynspire_allocator_report(
    alloc: *mut DynSpireAllocator,
) -> DynSpireAllocatorReport {
    if alloc.is_null() {
        return DynSpireAllocatorReport::default();
    }
    (*alloc).report()
}

// ---------------------------------------------------------------------------
// RC header
// ---------------------------------------------------------------------------

const fn round_up(x: usize, align: usize) -> usize {
    (x + align - 1) & !(align - 1)
}

/// Padding alignment for the header. DynSpire payloads are never more aligned
/// than this, so the header always sits at a fixed offset before the payload
/// and can be recovered from a payload pointer by subtraction.
const HEADER_PAD_ALIGN: usize = 32;

/// Size of the RC header as placed before every payload. Constant, so given a
/// payload pointer we recover the header by subtracting this.
pub const HEADER_SIZE: usize = round_up(
    std::mem::size_of::<DynSpireHeader>(),
    HEADER_PAD_ALIGN,
);

#[repr(C)]
pub struct DynSpireHeader {
    pub rc: AtomicUsize,
    pub type_index: u32,
    pub _pad: u32,
    pub drop_fn: Option<unsafe extern "C" fn(*mut c_void)>,
    pub allocator: *mut DynSpireAllocator,
    pub size: usize,
    pub align: usize,
}

// ---------------------------------------------------------------------------
// Vtable call helpers
// ---------------------------------------------------------------------------

#[inline]
unsafe fn v_alloc(alloc: *mut DynSpireAllocator, size: usize, align: usize) -> *mut u8 {
    let v = &*(*alloc).vtable;
    (v.alloc)((*alloc).ctx, size, align)
}

#[inline]
unsafe fn v_dealloc(alloc: *mut DynSpireAllocator, ptr: *mut u8, size: usize, align: usize) {
    let v = &*(*alloc).vtable;
    (v.dealloc)((*alloc).ctx, ptr, size, align);
}

#[inline]
unsafe fn v_realloc(
    alloc: *mut DynSpireAllocator,
    ptr: *mut u8,
    old_size: usize,
    new_size: usize,
    align: usize,
) -> *mut u8 {
    let v = &*(*alloc).vtable;
    (v.realloc)((*alloc).ctx, ptr, old_size, new_size, align)
}

#[inline]
unsafe fn header_of(payload: *mut u8) -> *mut DynSpireHeader {
    payload.sub(HEADER_SIZE) as *mut DynSpireHeader
}

// ---------------------------------------------------------------------------
// Allocation + reference counting
// ---------------------------------------------------------------------------

/// Internal allocation: allocate `[Header | payload]` and initialize the header.
/// Returns a pointer to the **payload** (what foreign code sees). `drop_fn`
/// runs when the refcount reaches zero (before dealloc); `type_index` is a
/// codegen-time constant used for diagnostics / drop dispatch if needed.
///
/// # Safety
///
/// `alloc` must be a valid, non-null [`DynSpireAllocator`] that outlives the
/// returned pointer. `align` must be a power of two and at most
/// [`HEADER_PAD_ALIGN`]. The returned pointer (or null on allocation failure)
/// is owned by the caller via `dynspire_release`.
pub unsafe fn dyn_alloc(
    alloc: *mut DynSpireAllocator,
    size: usize,
    align: usize,
    type_index: u32,
    drop_fn: Option<unsafe extern "C" fn(*mut c_void)>,
) -> *mut u8 {
    debug_assert!(align.is_power_of_two());
    debug_assert!(align <= HEADER_PAD_ALIGN);
    let total = HEADER_SIZE + size;
    let base = v_alloc(alloc, total, align);
    if base.is_null() {
        return std::ptr::null_mut();
    }
    let header = base as *mut DynSpireHeader;
    (*header).rc = AtomicUsize::new(1);
    (*header).type_index = type_index;
    (*header).drop_fn = drop_fn;
    (*header).allocator = alloc;
    (*header).size = size;
    (*header).align = align;
    base.add(HEADER_SIZE)
}

/// C-ABI allocation of a leaf buffer (no `drop_fn`, `type_index = 0`). Used by
/// foreign code and by codegen for owned byte/element buffers.
///
/// # Safety
///
/// `alloc` must be valid and outlive the returned pointer. `align` must be a
/// power of two. See [`dyn_alloc`] for ownership rules.
#[no_mangle]
pub unsafe extern "C" fn dynspire_alloc(
    alloc: *mut DynSpireAllocator,
    size: usize,
    align: usize,
) -> *mut u8 {
    dyn_alloc(alloc, size, align, 0, None)
}

/// C-ABI direct deallocation of a block previously obtained via `dynspire_alloc`
/// / `dynspire_realloc`. Does **not** run `drop_fn` and does **not** touch the
/// refcount — use [`dynspire_release`] for managed lifecycle.
///
/// # Safety
///
/// `ptr` must be a payload pointer from `dynspire_alloc`/`dynspire_realloc`
/// (or null). `size`/`align` must match the values passed at allocation. The
/// block is freed and must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn dynspire_dealloc(
    alloc: *mut DynSpireAllocator,
    ptr: *mut u8,
    size: usize,
    align: usize,
) {
    if ptr.is_null() {
        return;
    }
    let base = ptr.sub(HEADER_SIZE);
    let total = HEADER_SIZE + size;
    v_dealloc(alloc, base, total, align);
}

/// C-ABI resize. `ptr` is the current payload (or null to allocate fresh).
/// Updates the header's `size` so a subsequent [`dynspire_release`] deallocs
/// with the new total. Returns the new payload pointer (may differ from `ptr`).
///
/// # Safety
///
/// `ptr`/`old_size` must come from a prior allocation with `align`. `alloc`
/// must be the same allocator. The old block must not be used after this call.
#[no_mangle]
pub unsafe extern "C" fn dynspire_realloc(
    alloc: *mut DynSpireAllocator,
    ptr: *mut u8,
    old_size: usize,
    new_size: usize,
    align: usize,
) -> *mut u8 {
    if ptr.is_null() {
        return dyn_alloc(alloc, new_size, align, 0, None);
    }
    let old_base = ptr.sub(HEADER_SIZE);
    let old_total = HEADER_SIZE + old_size;
    let new_total = HEADER_SIZE + new_size;
    let new_base = v_realloc(alloc, old_base, old_total, new_total, align);
    if new_base.is_null() {
        return std::ptr::null_mut();
    }
    let header = new_base as *mut DynSpireHeader;
    (*header).size = new_size;
    new_base.add(HEADER_SIZE)
}

/// Increment the refcount.
///
/// # Safety
///
/// `ptr` must be a payload pointer from `dyn_alloc`/`dynspire_alloc` (or null).
#[no_mangle]
pub unsafe extern "C" fn dynspire_retain(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let header = header_of(ptr);
    (*header).rc.fetch_add(1, Ordering::Release);
}

/// Decrement the refcount. When it reaches zero, runs `drop_fn` (if any) then
/// reclaims the block via the allocator stored in the header.
///
/// # Safety
///
/// `ptr` must be a payload pointer from `dyn_alloc`/`dynspire_alloc` (or null).
/// After the final `release` (refcount to zero) the block is freed and the
/// pointer must not be used again.
#[no_mangle]
pub unsafe extern "C" fn dynspire_release(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let header = header_of(ptr);
    if (*header).rc.fetch_sub(1, Ordering::AcqRel) == 1 {
        let drop_fn = (*header).drop_fn;
        let alloc = (*header).allocator;
        let size = (*header).size;
        let align = (*header).align;
        if let Some(drop_fn) = drop_fn {
            drop_fn(ptr as *mut c_void);
        }
        let base = ptr.sub(HEADER_SIZE);
        let total = HEADER_SIZE + size;
        v_dealloc(alloc, base, total, align);
    }
}

// ---------------------------------------------------------------------------
// DynSpire types
// ---------------------------------------------------------------------------

/// Marker for types with a stable C layout. All implementors are `Copy`.
///
/// # Safety
///
/// A type implementing `ReprC` guarantees its `#[repr(C)]` layout is stable
/// and free of invalid bit patterns, so it can be projected across the FFI
/// boundary by any language with a C FFI. The trait has no unsafe methods;
/// the bound is purely a compile-time contract on layout.
#[allow(clippy::missing_safety_doc)]
pub unsafe trait ReprC: Copy {}

/// `&str` semantics — non-owning, read-only view. No allocator pointer.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DStr {
    pub ptr: *const u8,
    pub len: usize,
}

/// `&[T]` semantics — non-owning, read-only view. No allocator pointer.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DSlice<T: ReprC> {
    pub ptr: *const T,
    pub len: usize,
}

/// `String` semantics — owned, allocator pointer is the first field.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DString {
    pub allocator: *mut DynSpireAllocator,
    pub ptr: *mut u8,
    pub len: usize,
    pub cap: usize,
}

/// `Vec<T>` semantics — owned, allocator pointer is the first field.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DVec<T: ReprC> {
    pub allocator: *mut DynSpireAllocator,
    pub ptr: *mut T,
    pub len: usize,
    pub cap: usize,
}

/// `Option<T>` semantics. `tag == 0` is `None`; `value` is uninitialized.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DOption<T: ReprC> {
    pub tag: u8,
    pub _pad: [u8; 7],
    pub value: T,
}

// --- ReprC impls ---

unsafe impl ReprC for () {}
unsafe impl ReprC for bool {}
unsafe impl ReprC for u8 {}
unsafe impl ReprC for u16 {}
unsafe impl ReprC for u32 {}
unsafe impl ReprC for u64 {}
unsafe impl ReprC for i8 {}
unsafe impl ReprC for i16 {}
unsafe impl ReprC for i32 {}
unsafe impl ReprC for i64 {}
unsafe impl ReprC for f32 {}
unsafe impl ReprC for f64 {}
unsafe impl ReprC for usize {}
unsafe impl ReprC for isize {}

unsafe impl ReprC for DStr {}
unsafe impl<T: ReprC> ReprC for DSlice<T> {}
unsafe impl ReprC for DString {}
unsafe impl<T: ReprC> ReprC for DVec<T> {}
unsafe impl<T: ReprC> ReprC for DOption<T> {}

// ---------------------------------------------------------------------------
// DType constructors + owning guards
//
// `DVec`/`DString` own a host-allocator buffer. The low-level constructors
// (`DVec::new_in` / `DString::new_in`) allocate through a `*mut
// DynSpireAllocator` and are the single allocation path used by both the
// spier (via `DynSpireStateExt::new_dvec`) and the host (via the generated
// client `new_dvec`). The owning `OwnedDVec`/`OwnedDString` guards wrap the
// raw managed struct and release the buffer on `Drop`, so a returned DType is
// freed exactly once — by the side that received it.
// ---------------------------------------------------------------------------

impl<T: ReprC> DVec<T> {
    /// Allocate a `DVec` with `cap` element slots in `alloc`.
    ///
    /// The backing buffer carries a DynSpire RC header (see [`dyn_alloc`]), so
    /// it can be returned across the FFI boundary and released by the receiver
    /// via `dynspire_release`.
    pub fn new_in(alloc: &DynSpireAllocator, cap: usize) -> DVec<T> {
        let nbytes = cap * std::mem::size_of::<T>();
        let ptr = unsafe { dynspire_alloc(alloc as *const _ as *mut _, nbytes, std::mem::align_of::<T>()) };
        DVec {
            allocator: alloc as *const _ as *mut _,
            ptr: ptr as *mut T,
            len: 0,
            cap,
        }
    }

    /// Append `value`, growing the buffer (host allocator) as needed.
    pub fn push(&mut self, value: T) {
        if self.len == self.cap {
            let new_cap = if self.cap == 0 { 4 } else { self.cap * 2 };
            let old_nbytes = self.len * std::mem::size_of::<T>();
            let new_nbytes = new_cap * std::mem::size_of::<T>();
            let new_ptr = unsafe {
                dynspire_realloc(
                    self.allocator,
                    self.ptr as *mut u8,
                    old_nbytes,
                    new_nbytes,
                    std::mem::align_of::<T>(),
                )
            };
            if new_ptr.is_null() {
                panic!("DVec::push: realloc failed");
            }
            self.ptr = new_ptr as *mut T;
            self.cap = new_cap;
        }
        unsafe { std::ptr::write(self.ptr.add(self.len), value) };
        self.len += 1;
    }

    /// Number of elements.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the vector is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// View the contents without copying.
    pub fn as_slice(&self) -> &[T] {
        if self.ptr.is_null() || self.len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }
}

impl DString {
    /// Allocate a `DString` holding `s` in `alloc`.
    pub fn new_in(alloc: &DynSpireAllocator, s: &str) -> DString {
        let len = s.len();
        let ptr = unsafe { dynspire_alloc(alloc as *const _ as *mut _, len, 1) };
        if !ptr.is_null() && len > 0 {
            unsafe { std::ptr::copy_nonoverlapping(s.as_ptr(), ptr, len) };
        }
        DString {
            allocator: alloc as *const _ as *mut _,
            ptr,
            len,
            cap: len,
        }
    }

    /// Number of bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the string is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// View the contents as `&str` without copying.
    pub fn as_str(&self) -> &str {
        if self.ptr.is_null() || self.len == 0 {
            ""
        } else {
            unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(self.ptr, self.len)) }
        }
    }
}

impl DStr {
    /// Number of bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the view is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// View the contents as `&str` without copying.
    pub fn as_str(&self) -> &str {
        if self.ptr.is_null() || self.len == 0 {
            ""
        } else {
            unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(self.ptr, self.len)) }
        }
    }
}

impl<T: ReprC> DSlice<T> {
    /// Number of elements.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the view is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// View the contents without copying.
    pub fn as_slice(&self) -> &[T] {
        if self.ptr.is_null() || self.len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }
}

impl<T: ReprC> DOption<T> {
    /// A present optional value.
    pub fn some(value: T) -> Self {
        DOption {
            tag: 1,
            _pad: [0u8; 7],
            value,
        }
    }

    /// An absent optional value.
    pub fn none() -> Self {
        DOption {
            tag: 0,
            _pad: [0u8; 7],
            value: unsafe { std::mem::zeroed() },
        }
    }
}

impl<T: ReprC> From<Option<T>> for DOption<T> {
    fn from(o: Option<T>) -> Self {
        match o {
            Some(v) => DOption::some(v),
            None => DOption::none(),
        }
    }
}

/// Owning guard around [`DVec`]. Releases the backing buffer on `Drop`.
///
/// Spier methods return this type (built via `DynSpireStateExt::new_dvec`); the
/// generated dispatch converts it to a raw `DVec` (`into_raw`) before writing
/// the slots, so the buffer is *not* released on the spier side. The host
/// reconstructs an `OwnedDVec` from the received raw struct and releases it on
/// `Drop` — the single owner is the host caller.
pub struct OwnedDVec<T: ReprC> {
    inner: DVec<T>,
}

impl<T: ReprC> OwnedDVec<T> {
    /// Allocate a new owning vector in `alloc`.
    pub fn new_in(alloc: &DynSpireAllocator, cap: usize) -> Self {
        OwnedDVec {
            inner: DVec::new_in(alloc, cap),
        }
    }

    /// Consume the guard, returning the raw [`DVec`] without releasing it.
    ///
    /// Used by the spier dispatch to hand ownership to the host without a
    /// double-free.
    pub fn into_raw(self) -> DVec<T> {
        let d = self.inner;
        std::mem::forget(self);
        d
    }

    /// Wrap a raw [`DVec`] (received from the spier) into an owning guard.
    pub fn from_raw(inner: DVec<T>) -> Self {
        OwnedDVec { inner }
    }

    /// Number of elements.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the vector is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// View the contents without copying.
    pub fn as_slice(&self) -> &[T] {
        self.inner.as_slice()
    }
}

impl<T: ReprC> From<DVec<T>> for OwnedDVec<T> {
    fn from(inner: DVec<T>) -> Self {
        OwnedDVec { inner }
    }
}

impl<T: ReprC> std::ops::Deref for OwnedDVec<T> {
    type Target = DVec<T>;
    fn deref(&self) -> &DVec<T> {
        &self.inner
    }
}

impl<T: ReprC> std::ops::DerefMut for OwnedDVec<T> {
    fn deref_mut(&mut self) -> &mut DVec<T> {
        &mut self.inner
    }
}

impl<T: ReprC> Drop for OwnedDVec<T> {
    fn drop(&mut self) {
        unsafe { dynspire_release(self.inner.ptr as *mut u8) };
    }
}

/// Owning guard around [`DString`]. Releases the backing buffer on `Drop`.
pub struct OwnedDString {
    inner: DString,
}

impl OwnedDString {
    /// Allocate a new owning string in `alloc`.
    pub fn new_in(alloc: &DynSpireAllocator, s: &str) -> Self {
        OwnedDString {
            inner: DString::new_in(alloc, s),
        }
    }

    /// Consume the guard, returning the raw [`DString`] without releasing it.
    pub fn into_raw(self) -> DString {
        let d = self.inner;
        std::mem::forget(self);
        d
    }

    /// Wrap a raw [`DString`] (received from the spier) into an owning guard.
    pub fn from_raw(inner: DString) -> Self {
        OwnedDString { inner }
    }

    /// Number of bytes.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the string is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// View the contents as `&str` without copying.
    pub fn as_str(&self) -> &str {
        self.inner.as_str()
    }
}

impl From<DString> for OwnedDString {
    fn from(inner: DString) -> Self {
        OwnedDString { inner }
    }
}

impl std::ops::Deref for OwnedDString {
    type Target = DString;
    fn deref(&self) -> &DString {
        &self.inner
    }
}

impl std::ops::DerefMut for OwnedDString {
    fn deref_mut(&mut self) -> &mut DString {
        &mut self.inner
    }
}

impl Drop for OwnedDString {
    fn drop(&mut self) {
        unsafe { dynspire_release(self.inner.ptr) };
    }
}

/// Ergonomic access to the host allocator and DType constructors from within a
/// spier trait method.
///
/// Implemented for the spier state by the generated `impl_*_spier!` macro,
/// which recovers the allocator from the enclosing `__SpierState` (see the
/// macro for the `offset_of!` trick). Spier authors call `self.new_dvec(..)`
/// / `self.new_dstring(..)` and never touch the allocator pointer directly.
pub trait DynSpireStateExt {
    /// Raw host allocator pointer. Internal — prefer `new_dvec` / `new_dstring`.
    #[doc(hidden)]
    fn __dynspire_alloc(&self) -> *mut DynSpireAllocator;

    /// Build an owning `DVec` in the host allocator.
    fn new_dvec<T: ReprC>(&self, cap: usize) -> OwnedDVec<T> {
        OwnedDVec::new_in(unsafe { &*self.__dynspire_alloc() }, cap)
    }

    /// Build an owning `DString` in the host allocator.
    fn new_dstring(&self, s: &str) -> OwnedDString {
        OwnedDString::new_in(unsafe { &*self.__dynspire_alloc() }, s)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counters {
        allocs: AtomicUsize,
        frees: AtomicUsize,
        reallocs: AtomicUsize,
    }

    unsafe extern "C" fn t_alloc(ctx: *mut c_void, size: usize, align: usize) -> *mut u8 {
        let c = &*(ctx as *const Counters);
        c.allocs.fetch_add(1, Ordering::SeqCst);
        let layout = Layout::from_size_align(size, align).unwrap();
        std::alloc::alloc(layout)
    }
    unsafe extern "C" fn t_dealloc(ctx: *mut c_void, ptr: *mut u8, size: usize, align: usize) {
        let c = &*(ctx as *const Counters);
        c.frees.fetch_add(1, Ordering::SeqCst);
        let layout = Layout::from_size_align_unchecked(size, align);
        std::alloc::dealloc(ptr, layout);
    }
    unsafe extern "C" fn t_realloc(
        ctx: *mut c_void,
        ptr: *mut u8,
        old: usize,
        new: usize,
        align: usize,
    ) -> *mut u8 {
        let c = &*(ctx as *const Counters);
        c.reallocs.fetch_add(1, Ordering::SeqCst);
        let old_layout = Layout::from_size_align_unchecked(old, align);
        std::alloc::realloc(ptr, old_layout, new)
    }
    unsafe extern "C" fn t_drop_alloc(_ctx: *mut c_void) {}

    unsafe extern "C" fn t_report(_ctx: *mut c_void) -> DynSpireAllocatorReport {
        DynSpireAllocatorReport::default()
    }

    fn test_allocator() -> (DynSpireAllocator, *mut Counters) {
        let counters = Box::into_raw(Box::new(Counters {
            allocs: AtomicUsize::new(0),
            frees: AtomicUsize::new(0),
            reallocs: AtomicUsize::new(0),
        }));
        let vtable = Box::into_raw(Box::new(DynSpireAllocatorVtable {
            alloc: t_alloc,
            dealloc: t_dealloc,
            realloc: t_realloc,
            drop_allocator: t_drop_alloc,
            report: t_report,
        }));
        let alloc = DynSpireAllocator {
            vtable,
            ctx: counters as *mut c_void,
        };
        (alloc, counters)
    }

    unsafe fn free_test_allocator(alloc: DynSpireAllocator, counters: *mut Counters) {
        drop(Box::from_raw(counters));
        drop(Box::from_raw(alloc.vtable as *mut DynSpireAllocatorVtable));
    }

    #[test]
    fn alloc_release_leaf() {
        let (mut alloc, counters) = test_allocator();
        unsafe {
            let p = dyn_alloc(&mut alloc as *mut _, 16, 8, 0, None);
            assert!(!p.is_null());
            assert_eq!((*counters).allocs.load(Ordering::SeqCst), 1);
            ptr::write_bytes(p, 0xAB, 16);
            dynspire_release(p);
            assert_eq!((*counters).frees.load(Ordering::SeqCst), 1);
            free_test_allocator(alloc, counters);
        }
    }

    #[test]
    fn retain_keeps_alive_until_zero() {
        let (mut alloc, counters) = test_allocator();
        unsafe {
            let p = dyn_alloc(&mut alloc as *mut _, 8, 8, 0, None);
            dynspire_retain(p);
            dynspire_release(p); // rc 2 -> 1, still alive
            assert_eq!((*counters).frees.load(Ordering::SeqCst), 0);
            dynspire_release(p); // rc 1 -> 0, freed
            assert_eq!((*counters).frees.load(Ordering::SeqCst), 1);
            free_test_allocator(alloc, counters);
        }
    }

    static DROP_CALLED: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn my_drop(_p: *mut c_void) {
        DROP_CALLED.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn drop_fn_runs_on_zero() {
        DROP_CALLED.store(0, Ordering::SeqCst);
        let (mut alloc, counters) = test_allocator();
        unsafe {
            let p = dyn_alloc(&mut alloc as *mut _, 8, 8, 42, Some(my_drop));
            dynspire_retain(p);
            dynspire_release(p); // no drop yet
            assert_eq!(DROP_CALLED.load(Ordering::SeqCst), 0);
            dynspire_release(p); // drop + free
            assert_eq!(DROP_CALLED.load(Ordering::SeqCst), 1);
            assert_eq!((*counters).frees.load(Ordering::SeqCst), 1);
            free_test_allocator(alloc, counters);
        }
    }

    #[test]
    fn realloc_updates_header_and_rc() {
        let (mut alloc, counters) = test_allocator();
        unsafe {
            let p = dyn_alloc(&mut alloc as *mut _, 8, 8, 0, None);
            let p2 = dynspire_realloc(&mut alloc as *mut _, p, 8, 64, 8);
            assert!(!p2.is_null());
            assert_eq!((*counters).reallocs.load(Ordering::SeqCst), 1);
            let h = header_of(p2);
            assert_eq!((*h).size, 64);
            assert_eq!((*h).align, 8);
            assert_eq!((*h).rc.load(Ordering::SeqCst), 1);
            ptr::write_bytes(p2, 0xCD, 64);
            dynspire_release(p2);
            assert_eq!((*counters).frees.load(Ordering::SeqCst), 1);
            free_test_allocator(alloc, counters);
        }
    }

    #[test]
    fn realloc_null_allocates() {
        let (mut alloc, counters) = test_allocator();
        unsafe {
            let p = dynspire_realloc(&mut alloc as *mut _, ptr::null_mut(), 0, 32, 8);
            assert!(!p.is_null());
            assert_eq!((*counters).allocs.load(Ordering::SeqCst), 1);
            dynspire_release(p);
            free_test_allocator(alloc, counters);
        }
    }

    #[test]
    fn default_allocator_roundtrip() {
        let mut alloc = default_allocator();
        unsafe {
            let p = dynspire_alloc(&mut alloc as *mut _, 32, 8);
            assert!(!p.is_null());
            ptr::write_bytes(p, 0xAB, 32);
            dynspire_release(p);
        }
    }

    #[test]
    fn header_layout_constants() {
        assert!(HEADER_PAD_ALIGN.is_power_of_two());
        assert_eq!(HEADER_SIZE % HEADER_PAD_ALIGN, 0);
        // Header + an 8-byte payload must be recoverable by subtraction.
        let (mut alloc, counters) = test_allocator();
        unsafe {
            let p = dyn_alloc(&mut alloc as *mut _, 8, 8, 0, None);
            let h = header_of(p);
            assert_eq!((*h).size, 8);
            assert_eq!((*h).align, 8);
            dynspire_release(p);
            free_test_allocator(alloc, counters);
        }
    }

    #[test]
    fn debug_allocator_reports_occupation() {
        let mut alloc = debug_allocator();
        let before = alloc.report();
        let bytes_before = before.live_bytes;
        let allocs_before = before.live_allocations;
        unsafe {
            let p1 = dynspire_alloc(&mut alloc as *mut _, 32, 8);
            assert!(!p1.is_null());
            let p2 = dynspire_alloc(&mut alloc as *mut _, 7, 1);
            assert!(!p2.is_null());
            let mid = alloc.report();
            // Both payloads are allocated with header overhead included.
            assert!(mid.live_bytes >= bytes_before + 32 + 7);
            assert_eq!(mid.live_allocations, allocs_before + 2);
            assert!(mid.total_allocations >= 2);
            assert!(mid.peak_bytes >= mid.live_bytes);

            dynspire_release(p1);
            let after = alloc.report();
            assert!(after.live_bytes < mid.live_bytes);
            assert_eq!(after.live_allocations, allocs_before + 1);
            // Peak is monotonic.
            assert!(after.peak_bytes >= mid.peak_bytes);

            dynspire_release(p2);
            let done = alloc.report();
            assert_eq!(done.live_allocations, allocs_before);
            assert!(done.live_bytes <= bytes_before);
        }
    }

    #[test]
    fn default_allocator_report_is_zero() {
        let alloc = default_allocator();
        let r = alloc.report();
        assert_eq!(r.live_bytes, 0);
        assert_eq!(r.live_allocations, 0);
        assert_eq!(r.peak_bytes, 0);
        assert_eq!(r.total_allocations, 0);
    }

    // ------------------------------------------------------------------
    // DType round-trip tests
    // ------------------------------------------------------------------

    #[test]
    fn dvec_new_in_fields() {
        let alloc = default_allocator();
        let v = DVec::<u8>::new_in(&alloc, 4);
        assert_eq!(v.len, 0);
        assert_eq!(v.cap, 4);
        assert!(!v.ptr.is_null());
        assert_eq!(v.as_slice(), &[]);
    }

    #[test]
    fn dvec_push_and_as_slice() {
        let alloc = default_allocator();
        let mut v = DVec::<u8>::new_in(&alloc, 2);
        v.push(0xAA);
        v.push(0xBB);
        assert_eq!(v.len, 2);
        assert_eq!(v.as_slice(), &[0xAA, 0xBB]);
    }

    #[test]
    fn dvec_grow_via_realloc() {
        let alloc = default_allocator();
        let mut v = DVec::<u8>::new_in(&alloc, 1);
        for i in 0..16u8 {
            v.push(i);
        }
        assert_eq!(v.len, 16);
        assert!(v.cap >= 16);
        assert_eq!(v.as_slice(), &core::array::from_fn::<u8, 16, _>(|i| i as u8));
    }

    #[test]
    fn dstring_new_in_fields() {
        let alloc = default_allocator();
        let s = DString::new_in(&alloc, "hello");
        assert_eq!(s.len, 5);
        assert_eq!(s.as_str(), "hello");
    }

    #[test]
    fn dstring_empty() {
        let alloc = default_allocator();
        let s = DString::new_in(&alloc, "");
        assert_eq!(s.len, 0);
        assert!(s.as_str().is_empty());
    }

    #[test]
    fn dstr_as_str() {
        let data = b"test view";
        let view = DStr {
            ptr: data.as_ptr(),
            len: data.len(),
        };
        assert_eq!(view.len, 9);
        assert_eq!(view.as_str(), "test view");
    }

    #[test]
    fn dslice_as_slice() {
        let data: &[u8] = &[10, 20, 30];
        let view = DSlice::<u8> {
            ptr: data.as_ptr(),
            len: data.len(),
        };
        assert_eq!(view.len, 3);
        assert_eq!(view.as_slice(), &[10, 20, 30]);
    }

    #[test]
    fn doption_some_none_roundtrip() {
        let some: DOption<u8> = DOption::some(42);
        assert_eq!(some.tag, 1);
        assert_eq!(some.value, 42);

        let none: DOption<u8> = DOption::none();
        assert_eq!(none.tag, 0);
    }

    #[test]
    fn doption_from_option() {
        let some: DOption<u32> = Some(7u32).into();
        assert_eq!(some.tag, 1);
        assert_eq!(some.value, 7);

        let none: DOption<u32> = None::<u32>.into();
        assert_eq!(none.tag, 0);
    }

    #[test]
    fn dvec_into_raw_from_raw_roundtrip() {
        let (mut alloc, counters) = test_allocator();
        let mut v = DVec::<u8>::new_in(&alloc, 4);
        v.push(0xCC);
        v.push(0xDD);

        let raw: DVec<u8> = v;
        let bytes = raw.as_slice().to_vec();

        let v2 = DVec::<u8> {
            allocator: raw.allocator,
            ptr: raw.ptr,
            len: raw.len,
            cap: raw.cap,
        };
        assert_eq!(v2.as_slice(), bytes.as_slice());
        unsafe { dynspire_release(v2.ptr as *mut u8) };

        unsafe {
            assert_eq!((*counters).allocs.load(Ordering::SeqCst), (*counters).frees.load(Ordering::SeqCst));
            free_test_allocator(alloc, counters);
        }
    }

    #[test]
    fn dstring_into_raw_from_raw_roundtrip() {
        let (mut alloc, counters) = test_allocator();
        let s = DString::new_in(&alloc, "round-trip");

        let bytes = s.as_str().as_bytes().to_vec();
        let raw = DString {
            allocator: s.allocator,
            ptr: s.ptr,
            len: s.len,
            cap: s.cap,
        };
        assert_eq!(raw.as_str(), core::str::from_utf8(&bytes).unwrap());
        unsafe { dynspire_release(raw.ptr) };

        unsafe {
            assert_eq!((*counters).allocs.load(Ordering::SeqCst), (*counters).frees.load(Ordering::SeqCst));
            free_test_allocator(alloc, counters);
        }
    }

    #[test]
    fn owned_dvec_drop_releases() {
        let (mut alloc, counters) = test_allocator();
        {
            let mut ov = OwnedDVec::<u8>::new_in(&alloc, 4);
            ov.push(0x11);
            ov.push(0x22);
            assert_eq!(ov.len(), 2);
        }
        unsafe {
            assert_eq!((*counters).allocs.load(Ordering::SeqCst), (*counters).frees.load(Ordering::SeqCst));
            free_test_allocator(alloc, counters);
        }
    }

    #[test]
    fn owned_dstring_drop_releases() {
        let (mut alloc, counters) = test_allocator();
        {
            let os = OwnedDString::new_in(&alloc, "owned string");
            assert_eq!(os.as_str(), "owned string");
        }
        unsafe {
            assert_eq!((*counters).allocs.load(Ordering::SeqCst), (*counters).frees.load(Ordering::SeqCst));
            free_test_allocator(alloc, counters);
        }
    }

    #[test]
    fn dvec_full_roundtrip_with_growth() {
        let (mut alloc, counters) = test_allocator();
        let mut ov = OwnedDVec::<u64>::new_in(&alloc, 1);
        for i in 0..100u64 {
            ov.push(i);
        }
        assert_eq!(ov.len(), 100);
        unsafe { assert_eq!((*counters).allocs.load(Ordering::SeqCst), 1); }

        // Simulate spier → host handoff: take raw, forget the guard.
        let raw = DVec::<u64> {
            allocator: ov.inner.allocator,
            ptr: ov.inner.ptr,
            len: ov.inner.len,
            cap: ov.inner.cap,
        };
        std::mem::forget(ov);

        assert_eq!(raw.len, 100);
        unsafe { dynspire_release(raw.ptr as *mut u8) };

        unsafe {
            assert_eq!((*counters).allocs.load(Ordering::SeqCst), (*counters).frees.load(Ordering::SeqCst));
            free_test_allocator(alloc, counters);
        }
    }

    #[test]
    fn debug_allocator_leak_check() {
        let alloc = debug_allocator();
        let before = alloc.report();
        let live_before = before.live_bytes;
        let allocs_before = before.total_allocations;

        let mut ov = OwnedDVec::<u8>::new_in(&alloc, 64);
        for i in 0..64u8 {
            ov.push(i);
        }
        let mid = alloc.report();
        assert!(mid.live_bytes > live_before);
        assert!(mid.total_allocations > allocs_before);
        let live_after_alloc = mid.live_bytes;
        let alloc_count_after = mid.live_allocations;

        drop(ov);
        let after = alloc.report();
        assert_eq!(after.live_allocations, alloc_count_after - 1);
        assert!(after.live_bytes < live_after_alloc);
    }

    #[test]
    fn debug_allocator_peak_monotonic() {
        let alloc = debug_allocator();
        let before = alloc.report();
        let peak_before = before.peak_bytes;

        let mut ov = OwnedDVec::<u8>::new_in(&alloc, 32);
        for i in 0..32u8 {
            ov.push(i);
        }
        let peak1 = alloc.report().peak_bytes;
        assert!(peak1 >= peak_before);
        drop(ov);

        let peak2 = alloc.report().peak_bytes;
        assert!(peak2 >= peak1);
    }
}
