//! The kernel's locks, on `std::sync::Mutex`. A panic while holding a lock
//! does not poison it (the kernel's locks have no such notion).

use std::ops::{Deref, DerefMut};

pub struct Mutex<T>(std::sync::Mutex<T>);

pub struct MutexGuard<'a, T>(std::sync::MutexGuard<'a, T>);

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self(std::sync::Mutex::new(value))
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        MutexGuard(self.0.lock().unwrap_or_else(|p| p.into_inner()))
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.0.try_lock().ok().map(MutexGuard)
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut().unwrap_or_else(|p| p.into_inner())
    }
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

/// In the kernel a spin lock that disables interrupts; here a plain mutex.
pub type IrqLock<T> = Mutex<T>;
