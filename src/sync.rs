use std::sync::{Condvar, Mutex};

pub struct Semaphore {
    available: Mutex<usize>,
    cvar: Condvar,
    capacity: usize,
}

pub struct Guard<'a> {
    sem: &'a Semaphore,
}

impl Semaphore {
    pub fn new(n: usize) -> Self {
        Self {
            available: Mutex::new(n),
            cvar: Condvar::new(),
            capacity: n,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn available(&self) -> usize {
        *self.available.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn acquire(&self) -> Guard<'_> {
        let mut avail = self.available.lock().unwrap_or_else(|e| e.into_inner());
        while *avail == 0 {
            avail = self.cvar.wait(avail).unwrap_or_else(|e| e.into_inner());
        }
        *avail -= 1;
        Guard { sem: self }
    }

    pub fn try_acquire(&self) -> Option<Guard<'_>> {
        let mut avail = self.available.lock().unwrap_or_else(|e| e.into_inner());
        if *avail > 0 {
            *avail -= 1;
            Some(Guard { sem: self })
        } else {
            None
        }
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let mut avail = self
            .sem
            .available
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *avail += 1;
        self.sem.cvar.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn eight_threads_through_semaphore_of_two_never_exceed_two() {
        let sem = Arc::new(Semaphore::new(2));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let sem = Arc::clone(&sem);
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            handles.push(thread::spawn(move || {
                for _ in 0..5 {
                    let _guard = sem.acquire();
                    let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(cur, Ordering::SeqCst);
                    assert!(cur <= 2, "exceeded semaphore limit: {cur}");
                    thread::sleep(Duration::from_millis(5));
                    active.fetch_sub(1, Ordering::SeqCst);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 2);

        let g1 = sem.try_acquire();
        let g2 = sem.try_acquire();
        let g3 = sem.try_acquire();
        assert!(g1.is_some());
        assert!(g2.is_some());
        assert!(g3.is_none());
        drop(g1);
        assert!(sem.try_acquire().is_some());
    }
}
