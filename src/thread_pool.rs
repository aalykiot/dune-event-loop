use rayon::ThreadPool as Pool;
use rayon::ThreadPoolBuilder;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;

pub(crate) struct ThreadPool {
    /// The actual rayon thread-pool.
    inner: Pool,
    /// A tracker for all the pending tasks.
    pending_tasks: Arc<AtomicUsize>,
}

impl ThreadPool {
    /// Creates a new thread-pool with the requested threads.
    pub fn new(num_threads: usize) -> Self {
        // Number of threads should always be a positive non-zero number.
        assert!(num_threads > 0);

        let thread_pool = ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build()
            .unwrap();

        ThreadPool {
            inner: thread_pool,
            pending_tasks: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Puts the task into the thread-pool for execution.
    pub fn spawn<F>(&self, work: F, cancellation: mpsc::Receiver<()>)
    where
        F: FnOnce() + Send + 'static,
    {
        let pending = Arc::clone(&self.pending_tasks);

        self.pending_tasks.fetch_add(1, Ordering::Relaxed);
        self.inner.spawn(move || {
            // Start executing the task if there is no cancelation signal.
            if cancellation.try_recv().is_err() {
                work();
            }

            pending.fetch_sub(1, Ordering::Relaxed);
        });
    }

    /// Returns the number of the current active tasks.
    pub fn pending_count(&self) -> usize {
        self.pending_tasks.load(Ordering::Relaxed)
    }
}
