#![feature(thread_spawn_unchecked)]

use std::any::Any;
use std::cell::UnsafeCell;
use std::io;
use std::marker::PhantomData;
use std::mem;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread::{self, JoinHandle};

pub fn scope<'env, F, R>(f: F) -> Result<R, Box<[Box<dyn Any + Send>]>>
where
    for <'scope> F: FnOnce(&'scope Scope<'env>) -> R + 'env,
    R: 'env,
{
    let scope = Scope::new();
    let res = f(&scope);
    scope.join_all().map(|_| res)
}

unsafe impl<'env> Send for Scope<'env> {}
unsafe impl<'env> Sync for Scope<'env> {}

pub struct Scope<'env> {
    joins: Mutex<Vec<Arc<dyn Join + 'env>>>,
    _marker: PhantomData<&'env ()>,
}

impl<'env> Scope<'env> {
    pub fn builder(&self) -> ScopedThreadBuilder<'env, '_> {
        ScopedThreadBuilder(self, thread::Builder::new())
    }

    pub fn spawn<F, T>(&self, f: F) -> ScopedJoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'env,
        T: Send + 'env
    {
        self.builder().spawn(f).unwrap()
    }

    pub fn spawn_nested<'scope, F, T>(&'scope self, f: F) -> ScopedJoinHandle<T>
        where
            F: FnOnce(&'scope Scope<'env>) -> T + Send + 'env,
            T: Send + 'env
    {
        self.builder().spawn_nested(f).unwrap()
    }

    fn new() -> Self {
        Self {
            joins: Mutex::new(Vec::new()),
            _marker: PhantomData,
        }
    }

    fn join_all(self) -> Result<(), Box<[Box<dyn Any + Send + 'static>]>> {
        let panics = self.joins
            .lock()
            .unwrap()
            .drain(..)
            .filter_map(|handle| handle.join().err())
            .collect::<Vec<_>>();

        if panics.len() == 0 {
            Ok(())
        } else {
            Err(panics.into_boxed_slice())
        }
    }
}

impl<'env> Drop for Scope<'env> {
    fn drop(&mut self) {
        for handle in self.joins.lock().unwrap().drain(..) {
            let _ = handle.join();
        }
    }
}

pub struct ScopedThreadBuilder<'env, 'scope>(&'scope Scope<'env>, thread::Builder);

impl<'env, 'scope> ScopedThreadBuilder<'env, 'scope> {
    pub fn name(mut self, name: String) -> Self {
        self.1 = self.1.name(name);
        self
    }

    pub fn stack_size(mut self, size: usize) -> Self {
        self.1 = self.1.stack_size(size);
        self
    }

    pub fn spawn<F, T>(self, f: F) -> io::Result<ScopedJoinHandle<T>>
    where
        F: FnOnce() -> T + Send + 'env,
        T: Send + 'env,
    {
        let ScopedThreadBuilder(scope, builder) = self;

        let res = unsafe { builder.spawn_unchecked(f) };

        res.map(|handle| {
            let inner = Arc::new(UnsafeCell::new(JoinState::Unjoined(handle)));
            scope.joins.lock().unwrap().push(Arc::clone(&inner) as _);

            ScopedJoinHandle(inner)
        })
    }
}

pub struct ScopedJoinHandle<T>(Arc<UnsafeCell<JoinState<T>>>);

enum JoinState<T> {
    Unjoined(JoinHandle<T>),
    ScopeJoined(thread::Result<T>),
    Joined,
}

impl<T> ScopedJoinHandle<T> {
    pub fn join(self) -> thread::Result<T> {
        // this safe because...
        let state = unsafe { &mut *self.0.get() };

        match mem::replace(state, JoinState::Joined) {
            JoinState::Unjoined(handle) => handle.join(),
            JoinState::ScopeJoined(res) => res,
            JoinState::Joined => unreachable!(),
        }
    }
}

trait Join {
    fn join(self: Arc<Self>) -> thread::Result<()>;
}

impl<T> Join for UnsafeCell<JoinState<T>> {
    fn join(self: Arc<Self>) -> thread::Result<()> {
        let state = unsafe { &mut *self.get() };
        match mem::replace(state, JoinState::Joined) {
            JoinState::Unjoined(handle) => {
                let res = handle.join();
                // join handle is already dropped
                if Arc::strong_count(&self) == 1 {
                    res.map(|_| ())
                } else {
                    let ret = match res.as_ref() {
                        Ok(_) => Ok(()),
                        Err(_) => {
                            Err(Box::new(String::from("unspecified thread panicked")) as _)
                        }
                    };

                    *state = JoinState::ScopeJoined(res);

                    ret
                }
            },
            JoinState::Joined => Ok(()),
            JoinState::ScopeJoined(_) => unreachable!(),
        }
    }
}

struct WaitGroup;

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn simple() {
        let mut a = 0;
        scope(|scope| {
            scope.spawn(|| {
                a = 1;
            });
        })
        .unwrap();

        assert_eq!(a, 1);
    }

    #[test]
    fn multiple_writers() {
        let count = Mutex::new(0);

        scope(|scope| {
            scope.spawn(|| *count.lock().unwrap() += 1);
            scope.spawn(|| *count.lock().unwrap() += 1);
            scope.spawn(|| *count.lock().unwrap() += 1);
            scope.spawn(|| *count.lock().unwrap() += 1);
            scope.spawn(|| *count.lock().unwrap() += 1);
        })
        .unwrap();

        assert_eq!(*count.lock().unwrap(), 5);
    }

    #[test]
    fn manual_join() {
        let count = Mutex::new(0);

        scope(|scope| {
            let handles = (0..5)
                .map(|_| scope.spawn(|| *count.lock().unwrap() += 1))
                .collect::<Vec<_>>();

            for handle in handles {
                let _ = handle.join();
            }

            assert_eq!(*count.lock().unwrap(), 5);
        })
        .unwrap();
    }
}
