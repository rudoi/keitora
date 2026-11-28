//! Poison-tolerant locking for the app's shared state.

use std::sync::{Mutex, MutexGuard};

/// Lock `m`, recovering it if a previous holder panicked. The state behind the app's
/// mutexes is caches and bookkeeping that later calls revalidate, so a panic costs one
/// stale read instead of poisoning the lock and failing every later caller.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
