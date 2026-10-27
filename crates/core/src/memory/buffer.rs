use crate::memory::MemoryError;
use alloc::{alloc::Layout, slice, vec::Vec};
use core::{cmp, mem::ManuallyDrop};

/// A byte buffer implementation.
///
/// # Note
///
/// This is less efficient than the byte buffer implementation that is
/// based on actual OS provided virtual memory but it is a safe fallback
/// solution fitting any platform.
///
/// # Invariant
///
/// If the [`ByteBuffer`] is backed by a `Vec<u8>` all bytes in `len..capacity` are zero.
///
/// - Every allocation is zero-initialized for its whole capacity via `alloc_zeroed`.
/// - Nothing writes past `len`: slice access is bounded by `len` via [`ByteBuffer::data`]
///   and [`ByteBuffer::data_mut`], and raw access via `Memory::data_ptr` is bounded by
///   `Memory::data_size`, which is `len`.
/// - A [`ByteBuffer`] never shrinks.
///
/// This allows growing within the current capacity without touching any memory.
#[derive(Debug)]
pub struct ByteBuffer {
    /// The pointer to the underlying byte buffer.
    pub(super) ptr: *mut u8,
    /// The current length of the byte buffer.
    ///
    /// # Note
    ///
    /// - **Vec:** `vec.len()`
    /// - **Static:** The accessible subslice of the entire underlying static byte buffer.
    pub(super) len: usize,
    /// The capacity of the current allocation.
    ///
    /// # Note
    ///
    /// - **Vec**: `vec.capacity()`
    /// - **Static:** The total length of the underlying static byte buffer.
    capacity: usize,
    /// Whether the [`ByteBuffer`] was initialized from a `&'static [u8]` or a `Vec<u8>`.
    is_static: bool,
}

// # Safety
//
// `ByteBuffer` is essentially an `enum`` of `Vec<u8>` or `&'static mut [u8]`.
// Both of them are `Send` so this is sound.
unsafe impl Send for ByteBuffer {}

// # Safety
//
// `ByteBuffer` is essentially an `enum`` of `Vec<u8>` or `&'static mut [u8]`.
// Both of them are `Sync` so this is sound.
unsafe impl Sync for ByteBuffer {}

/// Decomposes the `Vec<u8>` into its raw components.
///
/// Returns the raw pointer to the underlying data, the length of
/// the vector (in bytes), and the allocated capacity of the
/// data (in bytes). These are the same arguments in the same
/// order as the arguments to [`Vec::from_raw_parts`].
///
/// # Safety
///
/// After calling this function, the caller is responsible for the
/// memory previously managed by the `Vec`. The only way to do
/// this is to convert the raw pointer, length, and capacity back
/// into a `Vec` with the [`Vec::from_raw_parts`] function, allowing
/// the destructor to perform the cleanup.
///
/// # Note
///
/// This utility method is required since [`Vec::into_raw_parts`] is
/// not yet stable unfortunately. (Date: 2024-03-14)
fn vec_into_raw_parts(vec: Vec<u8>) -> (*mut u8, usize, usize) {
    let mut vec = ManuallyDrop::new(vec);
    (vec.as_mut_ptr(), vec.len(), vec.capacity())
}

/// Copies `src` into the zero-initialized `dst`, skipping chunks of `src` that are all zero.
///
/// # Note
///
/// Writing a zero chunk into the zeroed `dst` would not change its contents but would
/// commit its pages. Skipping zero chunks keeps the pages of `dst` uncommitted that
/// correspond to pages of `src` which were never written. Reading never-written pages
/// of `src` does not commit them on systems that map such reads to a shared zero page.
///
/// # Panics (Debug)
///
/// If `src` and `dst` have different lengths.
fn copy_nonzero_chunks(src: &[u8], dst: &mut [u8]) {
    /// The chunk size; the smallest page size in common use. With larger pages (e.g. 16 KiB
    /// on macOS/arm64) a non-zero chunk commits the whole page that contains it.
    const CHUNK_SIZE: usize = 4096;
    static ZEROS: [u8; CHUNK_SIZE] = [0x00_u8; CHUNK_SIZE];
    debug_assert_eq!(src.len(), dst.len());
    for (src, dst) in src.chunks(CHUNK_SIZE).zip(dst.chunks_mut(CHUNK_SIZE)) {
        if src != &ZEROS[..src.len()] {
            dst.copy_from_slice(src);
        }
    }
}

impl ByteBuffer {
    /// Creates a new byte buffer with the given initial `size` in bytes.
    ///
    /// # Errors
    ///
    /// If the requested amount of heap bytes could not be allocated.
    pub fn new(size: usize) -> Result<Self, MemoryError> {
        let vec = Self::alloc_zeroed_vec(size)?;
        let (ptr, len, capacity) = vec_into_raw_parts(vec);
        Ok(Self {
            ptr,
            len,
            capacity,
            is_static: false,
        })
    }

    /// Allocates a zeroed `Vec<u8>` of `size` bytes without touching its pages.
    ///
    /// Unlike writing zeros, `alloc_zeroed` lets the system allocator hand out
    /// fresh OS pages lazily, so untouched pages cost no physical memory.
    fn alloc_zeroed_vec(size: usize) -> Result<Vec<u8>, MemoryError> {
        if size == 0 {
            return Ok(Vec::new());
        }
        let Ok(layout) = Layout::array::<u8>(size) else {
            return Err(MemoryError::OutOfSystemMemory);
        };
        // Safety: `layout` has a non-zero size.
        let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err(MemoryError::OutOfSystemMemory);
        }
        // Safety: `ptr` was allocated by the global allocator with the layout of
        //         `[u8; size]` and all `size` bytes are initialized to zero.
        Ok(unsafe { Vec::from_raw_parts(ptr, size, size) })
    }

    /// Creates a new static byte buffer with the given `size` in bytes.
    ///
    /// This will zero all the bytes in `buffer[0..initial_len`].
    ///
    /// # Errors
    ///
    /// If `size` is greater than the length of `buffer`.
    pub fn new_static(buffer: &'static mut [u8], size: usize) -> Result<Self, MemoryError> {
        let Some(bytes) = buffer.get_mut(..size) else {
            return Err(MemoryError::InvalidStaticBufferSize);
        };
        bytes.fill(0x00_u8);
        Ok(Self {
            ptr: buffer.as_mut_ptr(),
            len: size,
            capacity: buffer.len(),
            is_static: true,
        })
    }

    /// Grows the byte buffer to the given `new_size`.
    ///
    /// The newly added bytes will be zero initialized.
    ///
    /// `max_size` is the maximum size in bytes the byte buffer may ever grow to, if any.
    /// It bounds the capacity reserved for future growth.
    ///
    /// # Panics
    ///
    /// - If the current size of the [`ByteBuffer`] is larger than `new_size`.
    ///
    /// # Errors
    ///
    /// - If it is not possible to grow the [`ByteBuffer`] to `new_size`.
    ///     - `vec`: If the system allocator ran out of memory to allocate.
    ///     - `static`: If `new_size` is larger than it's the static buffer capacity.
    pub fn grow(&mut self, new_size: usize, max_size: Option<usize>) -> Result<(), MemoryError> {
        assert!(self.len() <= new_size);
        match self.get_vec() {
            Some(vec) => self.grow_vec(vec, new_size, max_size),
            None => self.grow_static(new_size),
        }
    }

    /// Grow the byte buffer to the given `new_size` when backed by a [`Vec`].
    fn grow_vec(
        &mut self,
        vec: ManuallyDrop<Vec<u8>>,
        new_size: usize,
        max_size: Option<usize>,
    ) -> Result<(), MemoryError> {
        debug_assert!(vec.len() <= new_size);
        if new_size <= self.capacity {
            // Invariant: all bytes in `len..capacity` are already zero.
            //
            // Thus growing within the current capacity does not need to write,
            // and thereby commit, any memory.
            self.len = new_size;
            return Ok(());
        }
        // Amortize the cost of growing the same way as `Vec` does.
        //
        // Since the new allocation is zero-initialized via `alloc_zeroed` its unused
        // capacity does not cost physical memory for allocations served with fresh
        // pages by the system allocator, unlike `Vec::resize` which writes zeros.
        // However, it does cost virtual address space which is limited on some
        // platforms. Therefore the capacity never exceeds the maximum size.
        let max_capacity = cmp::max(max_size.unwrap_or(usize::MAX), new_size);
        let amortized = cmp::min(
            cmp::max(new_size, self.capacity.saturating_mul(2)),
            max_capacity,
        );
        let mut new_vec = match Self::alloc_zeroed_vec(amortized) {
            Ok(new_vec) => new_vec,
            // Retry with the exact size before reporting an out of memory error.
            Err(_) if amortized > new_size => Self::alloc_zeroed_vec(new_size)?,
            Err(error) => return Err(error),
        };
        // Note: `new_vec` is zero-initialized so zero chunks need not be copied.
        copy_nonzero_chunks(&vec[..], &mut new_vec[..vec.len()]);
        // Frees the old allocation which is still owned by `vec`.
        drop(ManuallyDrop::into_inner(vec));
        let (ptr, _, capacity) = vec_into_raw_parts(new_vec);
        self.ptr = ptr;
        self.len = new_size;
        self.capacity = capacity;
        Ok(())
    }

    /// Grow the byte buffer to the given `new_size` when backed by a `&'static [u8]`.
    fn grow_static(&mut self, new_size: usize) -> Result<(), MemoryError> {
        if self.capacity < new_size {
            return Err(MemoryError::InvalidStaticBufferSize);
        }
        let len = self.len();
        self.len = new_size;
        self.data_mut()[len..new_size].fill(0x00_u8);
        Ok(())
    }

    /// Returns the length of the byte buffer in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns a shared slice to the bytes underlying to the byte buffer.
    pub fn data(&self) -> &[u8] {
        // # Safety
        //
        // The byte buffer is either backed by a `Vec<u8>` or a &'static [u8]`
        // which are both valid byte slices in the range `self.ptr[0..self.len]`.
        unsafe { slice::from_raw_parts(self.ptr, self.len) }
    }

    /// Returns an exclusive slice to the bytes underlying to the byte buffer.
    pub fn data_mut(&mut self) -> &mut [u8] {
        // # Safety
        //
        // The byte buffer is either backed by a `Vec<u8>` or a &'static [u8]`
        // which are both valid byte slices in the range `self.ptr[0..self.len]`.
        unsafe { slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    /// Returns the underlying `Vec<u8>` if the byte buffer is not backed by a static buffer.
    ///
    /// Otherwise returns `None`.
    ///
    /// # Note
    ///
    /// - The returned `Vec` will free its memory and thus the memory of the [`ByteBuffer`] if dropped.
    /// - The returned `Vec` is returned as [`ManuallyDrop`] to prevent its buffer from being freed
    ///   automatically upon going out of scope.
    fn get_vec(&mut self) -> Option<ManuallyDrop<Vec<u8>>> {
        if self.is_static {
            return None;
        }
        // Safety
        //
        // - At this point we are guaranteed that the byte buffer is backed by a `Vec`
        //   so it is safe to reconstruct the `Vec` by its raw parts.
        // - The returned `Vec` is returned as [`ManuallyDrop`] to prevent its buffer from being free
        //   upon going out of scope.
        let vec = unsafe { Vec::from_raw_parts(self.ptr, self.len, self.capacity) };
        Some(ManuallyDrop::new(vec))
    }
}

impl Drop for ByteBuffer {
    fn drop(&mut self) {
        self.get_vec().map(ManuallyDrop::into_inner);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// Asserts the [`ByteBuffer`] invariant that all bytes in `len..capacity` are zero.
    fn assert_zeroed_tail(buffer: &ByteBuffer) {
        assert!(!buffer.is_static);
        // Safety: the whole capacity of a `Vec` backed `ByteBuffer` is initialized
        //         since every allocation is zero-initialized via `alloc_zeroed`.
        let all = unsafe { slice::from_raw_parts(buffer.ptr, buffer.capacity) };
        assert!(
            all[buffer.len..].iter().all(|&byte| byte == 0x00_u8),
            "bytes in len..capacity must be zero (len = {}, capacity = {})",
            buffer.len,
            buffer.capacity,
        );
    }
    #[test]
    fn test_basic_allocation_deallocation() {
        let buffer = ByteBuffer::new(10).unwrap();
        assert_eq!(buffer.len(), 10);
        // Dropping the buffer should not cause UB.
    }

    #[test]
    fn test_basic_data_manipulation() {
        let mut buffer = ByteBuffer::new(10).unwrap();
        assert_eq!(buffer.len(), 10);
        let data = buffer.data(); // test we can read the data
        assert_eq!(data, &[0; 10]);
        let data = buffer.data_mut(); // test we can take a mutable reference to the data
        data[4] = 4; // test we can write to the data and it is not UB
        let data = buffer.data(); // test we can take a new reference to the data
        assert_eq!(data, &[0, 0, 0, 0, 4, 0, 0, 0, 0, 0]); // test we can read the data
        // test drop is okay
    }

    #[test]
    fn test_static_buffer_initialization() {
        static mut BUF: [u8; 10] = [7; 10];
        let buf = unsafe { &mut *core::ptr::addr_of_mut!(BUF) };
        let mut buffer = ByteBuffer::new_static(buf, 5).unwrap();
        assert_eq!(buffer.len(), 5);
        // Modifying the static buffer through ByteBuffer and checking its content.
        let data = buffer.data_mut();
        data[0] = 1;
        unsafe {
            assert_eq!(BUF[0], 1);
        }
    }

    #[test]
    fn test_growing_buffer() {
        let mut buffer = ByteBuffer::new(5).unwrap();
        buffer.grow(10, None).unwrap();
        assert_eq!(buffer.len(), 10);
        assert_eq!(buffer.data(), &[0; 10]);
    }

    #[test]
    fn test_growing_static() {
        static mut BUF: [u8; 10] = [7; 10];
        let buf = unsafe { &mut *core::ptr::addr_of_mut!(BUF) };
        let mut buffer = ByteBuffer::new_static(buf, 5).unwrap();
        assert_eq!(buffer.len(), 5);
        assert_eq!(buffer.data(), &[0; 5]);
        buffer.grow(8, None).unwrap();
        assert_eq!(buffer.len(), 8);
        assert_eq!(buffer.data(), &[0; 8]);
        buffer.grow(10, None).unwrap();
        assert_eq!(buffer.len(), 10);
        assert_eq!(buffer.data(), &[0; 10]);
    }

    #[test]
    fn test_static_buffer_overflow() {
        static mut BUF: [u8; 5] = [7; 5];
        let buf = unsafe { &mut *core::ptr::addr_of_mut!(BUF) };
        let mut buffer = ByteBuffer::new_static(buf, 5).unwrap();
        assert!(buffer.grow(10, None).is_err());
    }

    #[test]
    fn out_of_memory_works() {
        assert!(matches!(
            ByteBuffer::new(usize::MAX).unwrap_err(),
            MemoryError::OutOfSystemMemory
        ));
        let mut buffer = ByteBuffer::new(0).unwrap();
        assert!(matches!(
            buffer.grow(usize::MAX, None).unwrap_err(),
            MemoryError::OutOfSystemMemory
        ));
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.data().first(), None);
        assert!(buffer.grow(1, None).is_ok());
        assert!(matches!(
            buffer.grow(usize::MAX, None).unwrap_err(),
            MemoryError::OutOfSystemMemory
        ));
        assert_eq!(buffer.len(), 1);
        assert_eq!(buffer.data().first(), Some(&0x00_u8));
    }

    #[test]
    fn grow_from_empty() {
        let mut buffer = ByteBuffer::new(0).unwrap();
        buffer.grow(0, None).unwrap();
        assert_eq!(buffer.len(), 0);
        buffer.grow(1, None).unwrap();
        assert_eq!(buffer.data(), &[0x00_u8]);
    }

    #[test]
    fn grow_matches_vec_model() {
        // Checks the contents after each growth step against a plain `Vec<u8>`.
        //
        // The step sizes cover growing by nothing, by a single byte and by large
        // amounts, so that both growing within and beyond the current capacity
        // are exercised regardless of the buffer's allocation strategy.
        let mut buffer = ByteBuffer::new(3).unwrap();
        let mut model = alloc::vec![0x00_u8; 3];
        for (step, new_size) in [3, 4, 7, 8, 9, 31, 32, 33, 1000].into_iter().enumerate() {
            buffer.grow(new_size, None).unwrap();
            model.resize(new_size, 0x00_u8);
            assert_eq!(
                buffer.data(),
                &model[..],
                "mismatch after growing to {new_size}"
            );
            assert_zeroed_tail(&buffer);
            // Write the first and last byte so that later growth steps must preserve them.
            let marker = step as u8 + 1;
            buffer.data_mut()[0] = marker;
            buffer.data_mut()[new_size - 1] = marker;
            model[0] = marker;
            model[new_size - 1] = marker;
        }
        assert_eq!(buffer.data(), &model[..]);
    }

    #[test]
    fn grow_within_capacity_keeps_allocation() {
        let mut buffer = ByteBuffer::new(4).unwrap();
        // Growing beyond the capacity reallocates with amortized capacity.
        buffer.grow(5, None).unwrap();
        assert_eq!(buffer.capacity, 8);
        assert_zeroed_tail(&buffer);
        buffer.data_mut()[4] = 0xFF;
        let ptr = buffer.ptr;
        // Growing within the capacity neither reallocates nor touches memory.
        buffer.grow(8, None).unwrap();
        assert_eq!(buffer.ptr, ptr);
        assert_eq!(buffer.data(), &[0, 0, 0, 0, 0xFF, 0, 0, 0]);
        assert_zeroed_tail(&buffer);
    }

    #[test]
    fn grow_capacity_is_capped_at_max_size() {
        let mut buffer = ByteBuffer::new(4).unwrap();
        // Doubling would yield a capacity of 8 but the maximum size is 6.
        buffer.grow(5, Some(6)).unwrap();
        assert_eq!(buffer.capacity, 6);
        assert_zeroed_tail(&buffer);
        // Growing to the maximum size stays within the capacity.
        let ptr = buffer.ptr;
        buffer.grow(6, Some(6)).unwrap();
        assert_eq!(buffer.ptr, ptr);
        assert_eq!(buffer.data(), &[0; 6]);
        // Without a maximum size the capacity is amortized as usual.
        let mut buffer = ByteBuffer::new(4).unwrap();
        buffer.grow(5, None).unwrap();
        assert_eq!(buffer.capacity, 8);
    }

    #[test]
    fn grow_beyond_capacity_preserves_sparse_contents() {
        // Spans several 4 KiB chunks so that zero chunks are skipped while
        // non-zero chunks, including a partial last chunk, are copied.
        let mut buffer = ByteBuffer::new(10_000).unwrap();
        let marks = [0, 4095, 4096, 9_999];
        for (i, &pos) in marks.iter().enumerate() {
            buffer.data_mut()[pos] = i as u8 + 1;
        }
        buffer.grow(20_000, None).unwrap();
        assert_eq!(buffer.len(), 20_000);
        for (i, byte) in buffer.data().iter().enumerate() {
            let expected = match marks.iter().position(|&pos| pos == i) {
                Some(n) => n as u8 + 1,
                None => 0x00_u8,
            };
            assert_eq!(*byte, expected, "mismatch at byte {i}");
        }
        assert_zeroed_tail(&buffer);
    }
}
