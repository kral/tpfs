//! Crash points: named places right before tpfs changes the disk or its journal.
//!
//! In a normal build [`point`] does nothing. With the `test-support` feature (and in tpfs-core's
//! own unit tests) a test can [`run`] a piece of work, get the list of the points it passed, and
//! run it again stopping at the n-th one — as a crash or a power loss there would: nothing after
//! the point happens, and no error path of the interrupted code gets to tidy up. Because the
//! points sit in the few functions every change goes through (the journal's append and the
//! confined file operations), new code is covered without listing anything by hand.

/// A crash point named `at`: here the next change to the disk or the journal is about to happen.
#[inline(always)]
pub(crate) fn point(at: &'static str) {
	point_with(at, || {});
}

/// A crash point that, when the crash happens at it, first runs `torn`: what a write cut short
/// leaves behind (half a journal line).
#[inline(always)]
pub(crate) fn point_with(at: &'static str, torn: impl FnOnce()) {
	#[cfg(any(test, feature = "test-support"))]
	imp::point(at, torn);
	#[cfg(not(any(test, feature = "test-support")))]
	let _ = (at, torn);
}

/// Whether a change just made is to be flushed to the disk. Always, except inside a test's [`run`]:
/// a simulated crash loses nothing that was written, so the flushes (by far the slowest part of
/// a run, and there are thousands of runs) would buy nothing there.
#[inline(always)]
pub(crate) fn durable() -> bool {
	#[cfg(any(test, feature = "test-support"))]
	return !imp::recording();
	#[cfg(not(any(test, feature = "test-support")))]
	true
}

#[cfg(any(test, feature = "test-support"))]
pub use imp::{Crashed, run};

#[cfg(any(test, feature = "test-support"))]
mod imp {
	use std::{
		cell::RefCell,
		panic::{self, AssertUnwindSafe},
	};

	struct Recording {
		hits: Vec<&'static str>,
		crash_at: Option<usize>,
	}

	thread_local! {
		static RECORDING: RefCell<Option<Recording>> = const { RefCell::new(None) };
	}

	/// Why [`run`] stopped: the crash point's name and its position among the points passed.
	#[derive(Clone, Copy, Debug, PartialEq, Eq)]
	pub struct Crashed {
		pub at: &'static str,
		pub index: usize,
	}

	pub(super) fn recording() -> bool {
		RECORDING.with(|r| r.borrow().is_some())
	}

	pub(super) fn point(at: &'static str, torn: impl FnOnce()) {
		let crash = RECORDING.with(|r| {
			let mut r = r.borrow_mut();
			let rec = r.as_mut()?;
			rec.hits.push(at);
			let index = rec.hits.len() - 1;
			(rec.crash_at == Some(index)).then_some(Crashed { at, index })
		});
		if let Some(crashed) = crash {
			torn();
			// Unwinds without the panic hook (no message): the work stops here, as the process would.
			panic::resume_unwind(Box::new(crashed));
		}
	}

	/// Run `work` on this thread, recording every crash point it passes; with `crash_at`, stop it
	/// at the point with that position (counted from 0), as a crash would. Returns what `work`
	/// returned, or where it was stopped, and the names of the points passed, in order. A panic of
	/// `work` itself is passed on.
	pub fn run<T>(crash_at: Option<usize>, work: impl FnOnce() -> T) -> (Result<T, Crashed>, Vec<&'static str>) {
		let outer = RECORDING.with(|r| r.borrow_mut().replace(Recording { hits: Vec::new(), crash_at }));
		let result = panic::catch_unwind(AssertUnwindSafe(work));
		let hits = RECORDING.with(|r| std::mem::replace(&mut *r.borrow_mut(), outer)).map_or_else(Vec::new, |r| r.hits);
		match result {
			Ok(value) => (Ok(value), hits),
			Err(payload) => match payload.downcast::<Crashed>() {
				Ok(crashed) => (Err(*crashed), hits),
				Err(other) => panic::resume_unwind(other),
			},
		}
	}
}
