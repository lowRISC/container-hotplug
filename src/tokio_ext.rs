use std::ops::{Deref, DerefMut};
use tokio::task::JoinHandle;

pub struct JoinHandleGuard<T>(JoinHandle<T>);

pub trait WithJoinHandleGuard {
    type Output;
    fn guard(self) -> JoinHandleGuard<Self::Output>;
}

impl<T> WithJoinHandleGuard for JoinHandle<T> {
    type Output = T;
    fn guard(self) -> JoinHandleGuard<Self::Output> {
        JoinHandleGuard(self)
    }
}

impl<T> Deref for JoinHandleGuard<T> {
    type Target = JoinHandle<T>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for JoinHandleGuard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> Drop for JoinHandleGuard<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}
