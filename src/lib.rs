//! An allocator that can track and limit memory usage.
//!
//! **[Crates.io](https://crates.io/crates/cap) │ [Repo](https://github.com/alecmocatta/cap)**
//!
//! This crate provides a generic allocator that wraps another allocator, tracking memory usage and enabling limits to be set.
//!
//! # Example
//!
//! It can be used by declaring a static and marking it with the `#[global_allocator]` attribute:
//!
//! ```
//! use std::alloc;
//! use cap::Cap;
//!
//! #[global_allocator]
//! static ALLOCATOR: Cap<alloc::System> = Cap::new(alloc::System, usize::max_value());
//!
//! fn main() {
//!     // Set the limit to 30MiB.
//!     ALLOCATOR.set_limit(30 * 1024 * 1024).unwrap();
//!     // ...
//!     println!("Currently allocated: {}B", ALLOCATOR.allocated());
//! }
//! ```

#![cfg_attr(feature = "nightly", feature(allocator_api))]
#![cfg_attr(
	all(test, feature = "nightly"),
	feature(try_reserve, test, custom_test_frameworks)
)]
#![cfg_attr(all(test, feature = "nightly"), test_runner(tests::runner))]
#![warn(
	missing_copy_implementations,
	missing_debug_implementations,
	missing_docs,
	trivial_casts,
	trivial_numeric_casts,
	unused_import_braces,
	unused_qualifications,
	unused_results,
	clippy::pedantic
)] // from https://github.com/rust-unofficial/patterns/blob/master/anti_patterns/deny-warnings.md
#![allow()]

#[cfg(feature = "nightly")]
use std::alloc::{Alloc, AllocErr, CannotReallocInPlace};
use std::{
	alloc::{GlobalAlloc, Layout}, ptr, sync::atomic::{AtomicUsize, Ordering}
};

/// A struct that wraps another allocator and limits the number of bytes that can be allocated.
#[derive(Debug)]
pub struct Cap<H> {
	allocator: H,
	remaining: AtomicUsize,
	limit: AtomicUsize,
}

impl<H> Cap<H> {
	/// Create a new allocator, wrapping the supplied allocator and enforcing the specified limit.
	///
	/// For no limit, simply set the limit to the theoretical maximum `usize::max_value()`.
	pub const fn new(allocator: H, limit: usize) -> Self {
		Self {
			allocator,
			remaining: AtomicUsize::new(limit),
			limit: AtomicUsize::new(limit),
		}
	}

	/// Return the number of bytes remaining within the limit.
	///
	/// i.e. `limit - allocated`
	pub fn remaining(&self) -> usize {
		self.remaining.load(Ordering::Relaxed)
	}

	/// Return the limit in bytes.
	pub fn limit(&self) -> usize {
		self.limit.load(Ordering::Relaxed)
	}

	/// Set the limit in bytes.
	///
	/// For no limit, simply set the limit to the theoretical maximum `usize::max_value()`.
	///
	/// This method will return `Err` if the specified limit is less than the number of bytes already allocated.
	pub fn set_limit(&self, limit: usize) -> Result<(), ()> {
		loop {
			let limit_old = self.limit.load(Ordering::Relaxed);
			if limit < limit_old {
				if self
					.remaining
					.fetch_sub(limit_old - limit, Ordering::Relaxed)
					< limit_old - limit
				{
					let _ = self
						.remaining
						.fetch_add(limit_old - limit, Ordering::Relaxed);
					break Err(());
				}
				if self
					.limit
					.compare_and_swap(limit_old, limit, Ordering::Relaxed)
					!= limit_old
				{
					continue;
				}
			} else {
				if self
					.limit
					.compare_and_swap(limit_old, limit, Ordering::Relaxed)
					!= limit_old
				{
					continue;
				}
				let _ = self
					.remaining
					.fetch_add(limit - limit_old, Ordering::Relaxed);
			}
			break Ok(());
		}
	}

	/// Return the number of bytes allocated. Always less than the limit.
	pub fn allocated(&self) -> usize {
		// Make reasonable effort to get valid output
		loop {
			let limit_old = self.limit.load(Ordering::SeqCst);
			let remaining = self.remaining.load(Ordering::SeqCst);
			let limit = self.limit.load(Ordering::SeqCst);
			if limit_old == limit && limit >= remaining {
				break limit - remaining;
			}
		}
	}
}

unsafe impl<H> GlobalAlloc for Cap<H>
where
	H: GlobalAlloc,
{
	unsafe fn alloc(&self, l: Layout) -> *mut u8 {
		let size = l.size();
		let res = if self.remaining.fetch_sub(size, Ordering::Acquire) >= size {
			self.allocator.alloc(l)
		} else {
			ptr::null_mut()
		};
		if res.is_null() {
			let _ = self.remaining.fetch_add(size, Ordering::Release);
		}
		res
	}
	unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
		let size = layout.size();
		self.allocator.dealloc(ptr, layout);
		let _ = self.remaining.fetch_add(size, Ordering::Release);
	}
	unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
		let size = l.size();
		let res = if self.remaining.fetch_sub(size, Ordering::Acquire) >= size {
			self.allocator.alloc_zeroed(l)
		} else {
			ptr::null_mut()
		};
		if res.is_null() {
			let _ = self.remaining.fetch_add(size, Ordering::Release);
		}
		res
	}
	unsafe fn realloc(&self, ptr: *mut u8, old_l: Layout, new_s: usize) -> *mut u8 {
		let new_l = Layout::from_size_align_unchecked(new_s, old_l.align());
		let (old_size, new_size) = (old_l.size(), new_l.size());
		if new_size > old_size {
			let res = if self
				.remaining
				.fetch_sub(new_size - old_size, Ordering::Acquire)
				>= new_size - old_size
			{
				self.allocator.realloc(ptr, old_l, new_s)
			} else {
				ptr::null_mut()
			};
			if res.is_null() {
				let _ = self
					.remaining
					.fetch_add(new_size - old_size, Ordering::Release);
			}
			res
		} else {
			let res = self.allocator.realloc(ptr, old_l, new_s);
			if !res.is_null() {
				let _ = self
					.remaining
					.fetch_add(old_size - new_size, Ordering::Release);
			}
			res
		}
	}
}

#[cfg(feature = "nightly")]
unsafe impl<H> Alloc for Cap<H>
where
	H: Alloc,
{
	unsafe fn alloc(&mut self, l: Layout) -> Result<ptr::NonNull<u8>, AllocErr> {
		let size = self.allocator.usable_size(&l).1;
		let res = if self.remaining.fetch_sub(size, Ordering::Acquire) >= size {
			self.allocator.alloc(l)
		} else {
			Err(AllocErr)
		};
		if res.is_err() {
			let _ = self.remaining.fetch_add(size, Ordering::Release);
		}
		res
	}
	unsafe fn dealloc(&mut self, item: ptr::NonNull<u8>, l: Layout) {
		let size = self.allocator.usable_size(&l).1;
		self.allocator.dealloc(item, l);
		let _ = self.remaining.fetch_add(size, Ordering::Release);
	}
	fn usable_size(&self, layout: &Layout) -> (usize, usize) {
		self.allocator.usable_size(layout)
	}
	unsafe fn realloc(
		&mut self, ptr: ptr::NonNull<u8>, old_l: Layout, new_s: usize,
	) -> Result<ptr::NonNull<u8>, AllocErr> {
		let new_l = Layout::from_size_align_unchecked(new_s, old_l.align());
		let (old_size, new_size) = (
			self.allocator.usable_size(&old_l).1,
			self.allocator.usable_size(&new_l).1,
		);
		if new_size > old_size {
			let res = if self
				.remaining
				.fetch_sub(new_size - old_size, Ordering::Acquire)
				>= new_size - old_size
			{
				self.allocator.realloc(ptr, old_l, new_s)
			} else {
				Err(AllocErr)
			};
			if res.is_err() {
				let _ = self
					.remaining
					.fetch_add(new_size - old_size, Ordering::Release);
			}
			res
		} else {
			let res = self.allocator.realloc(ptr, old_l, new_s);
			if res.is_ok() {
				let _ = self
					.remaining
					.fetch_add(old_size - new_size, Ordering::Release);
			}
			res
		}
	}
	unsafe fn alloc_zeroed(&mut self, l: Layout) -> Result<ptr::NonNull<u8>, AllocErr> {
		let size = self.allocator.usable_size(&l).1;
		let res = if self.remaining.fetch_sub(size, Ordering::Acquire) >= size {
			self.allocator.alloc_zeroed(l)
		} else {
			Err(AllocErr)
		};
		if res.is_err() {
			let _ = self.remaining.fetch_add(size, Ordering::Release);
		}
		res
	}
	unsafe fn grow_in_place(
		&mut self, ptr: ptr::NonNull<u8>, old_l: Layout, new_s: usize,
	) -> Result<(), CannotReallocInPlace> {
		let new_l = Layout::from_size_align(new_s, old_l.align()).unwrap();
		let (old_size, new_size) = (
			self.allocator.usable_size(&old_l).1,
			self.allocator.usable_size(&new_l).1,
		);
		let res = if self
			.remaining
			.fetch_sub(new_size - old_size, Ordering::Acquire)
			>= new_size - old_size
		{
			self.allocator.grow_in_place(ptr, old_l, new_s)
		} else {
			Err(CannotReallocInPlace)
		};
		if res.is_err() {
			let _ = self
				.remaining
				.fetch_add(new_size - old_size, Ordering::Release);
		}
		res
	}
	unsafe fn shrink_in_place(
		&mut self, ptr: ptr::NonNull<u8>, old_l: Layout, new_s: usize,
	) -> Result<(), CannotReallocInPlace> {
		let new_l = Layout::from_size_align(new_s, old_l.align()).unwrap();
		let (old_size, new_size) = (
			self.allocator.usable_size(&old_l).1,
			self.allocator.usable_size(&new_l).1,
		);
		let res = self.allocator.shrink_in_place(ptr, old_l, new_s);
		if res.is_ok() {
			let _ = self
				.remaining
				.fetch_add(old_size - new_size, Ordering::Release);
		}
		res
	}
}

#[cfg(test)]
mod tests {
	#[cfg(all(test, feature = "nightly"))]
	extern crate test;
	#[cfg(all(test, feature = "nightly"))]
	use std::collections::TryReserveError;
	use std::{alloc, thread};
	#[cfg(all(test, feature = "nightly"))]
	use test::{TestDescAndFn, TestFn};

	use super::Cap;

	#[global_allocator]
	static A: Cap<alloc::System> = Cap::new(alloc::System, usize::max_value());

	#[cfg(all(test, feature = "nightly"))]
	pub fn runner(tests: &[&TestDescAndFn]) {
		for test in tests {
			if let TestFn::StaticTestFn(test_fn) = test.testfn {
				test_fn();
			} else {
				unimplemented!();
			}
		}
	}

	#[test]
	fn concurrent() {
		let allocated = A.allocated();
		for _ in 0..100 {
			let threads = (0..100)
				.map(|_| {
					thread::spawn(|| {
						for i in 0..1000 {
							let _ = (0..i).collect::<Vec<u32>>();
							let _ = (0..i).flat_map(std::iter::once).collect::<Vec<u32>>();
						}
					})
				})
				.collect::<Vec<_>>();
			threads
				.into_iter()
				.for_each(|thread| thread.join().unwrap());
			let allocated2 = A.allocated();
			if cfg!(all(test, feature = "nightly")) {
				assert_eq!(allocated, allocated2);
			}
		}
	}

	#[cfg(all(test, feature = "nightly"))]
	#[test]
	fn limit() {
		A.set_limit(A.allocated() + 30 * 1024 * 1024).unwrap();
		for _ in 0..10 {
			let mut vec = Vec::<u8>::with_capacity(0);
			if let Err(TryReserveError::AllocError { .. }) =
				vec.try_reserve_exact(30 * 1024 * 1024 + 1)
			{
			} else {
				A.set_limit(usize::max_value()).unwrap();
				panic!("{}", A.remaining())
			};
			assert_eq!(vec.try_reserve_exact(30 * 1024 * 1024), Ok(()));
		}
	}
}
