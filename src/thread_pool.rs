use rayon::ThreadPool as Pool;
use rayon::ThreadPoolBuilder;
use std::sync::mpsc;

/// A wrapper around a rayon thread-pool.
pub(crate) struct ThreadPool(Pool);

impl ThreadPool {
    /// Creates a new thread-pool with the requested threads.
    pub fn new(num_threads: usize) -> Self {
        // Number of threads should always be a positive non-zero number.
        assert!(num_threads > 0);

        let thread_pool = ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build()
            .unwrap();

        ThreadPool(thread_pool)
    }

    /// Puts the task into the thread-pool for execution.
    pub fn spawn<F>(&self, work: F, cancellation: mpsc::Receiver<()>)
    where
        F: FnOnce() + Send + 'static,
    {
        self.0.spawn(move || {
            // Start executing the task if there is no cancelation signal.
            if cancellation.try_recv().is_err() {
                work();
            }
        });
    }
}
