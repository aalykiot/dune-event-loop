use anyhow::Result;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use threadpool::ThreadPool;

type Index = u32;

type TaskResult = Option<Result<Vec<u8>>>;

enum Action {
    NewTimer(Index, Duration, Box<dyn FnOnce()>),
    RemoveTimer(Index),
    SpawnTask(
        Index,
        Box<dyn FnOnce() -> TaskResult + Send>,
        Option<Box<dyn FnOnce(TaskResult)>>,
    ),
}

enum Event {
    ThreadPool(Index, TaskResult),
}

pub struct EventLoop {
    index: Rc<Cell<Index>>,
    timer_callbacks: HashMap<Index, Box<dyn FnOnce()>>,
    timer_queue: BTreeMap<Instant, Index>,
    action_queue: mpsc::Receiver<Action>,
    action_queue_empty: Rc<Cell<bool>>,
    action_dispatcher: Rc<mpsc::Sender<Action>>,
    thread_pool: ThreadPool,
    task_callbacks: HashMap<Index, Box<dyn FnOnce(TaskResult)>>,
    event_dispatcher: Arc<Mutex<mpsc::Sender<Event>>>,
    event_queue: mpsc::Receiver<Event>,
    pending_tasks: u32,
}

impl EventLoop {
    pub fn new() -> Self {
        let (action_dispatcher, action_queue) = mpsc::channel();
        let (event_dispatcher, event_queue) = mpsc::channel();

        EventLoop {
            index: Rc::new(Cell::new(1)),
            timer_callbacks: HashMap::new(),
            timer_queue: BTreeMap::new(),
            action_queue,
            action_queue_empty: Rc::new(Cell::new(true)),
            action_dispatcher: Rc::new(action_dispatcher),
            thread_pool: ThreadPool::new(4),
            task_callbacks: HashMap::new(),
            event_dispatcher: Arc::new(Mutex::new(event_dispatcher)),
            event_queue,
            pending_tasks: 0,
        }
    }

    pub fn handle(&self) -> LoopHandle {
        LoopHandle {
            index: self.index.clone(),
            dispatcher: self.action_dispatcher.clone(),
            actions_queue_empty: self.action_queue_empty.clone(),
        }
    }

    pub fn has_pending_events(&self) -> bool {
        !(self.timer_queue.is_empty() && self.action_queue_empty.get() && self.pending_tasks == 0)
    }

    pub fn tick(&mut self) {
        self.prepare();
        self.run_timers();
        self.run_poll();
    }

    fn prepare(&mut self) {
        while let Ok(action) = self.action_queue.try_recv() {
            match action {
                Action::NewTimer(index, delay, cb) => self.ev_new_timer(index, delay, cb),
                Action::RemoveTimer(index) => self.ev_remove_timer(&index),
                Action::SpawnTask(index, task, task_cb) => self.ev_spawn_task(index, task, task_cb),
            };
        }
        self.action_queue_empty.set(true);
    }

    fn run_timers(&mut self) {
        let timers_to_remove: Vec<Instant> = self
            .timer_queue
            .range(..Instant::now())
            .map(|(k, _)| *k)
            .collect();

        timers_to_remove.iter().for_each(|key| {
            let index = match self.timer_queue.remove(key) {
                Some(index) => index,
                None => return,
            };
            if let Some(callback) = self.timer_callbacks.remove(&index) {
                (callback)();
            }
        });

        self.prepare();
    }

    fn run_poll(&mut self) {
        // Based on what resources the event-loop is currently running will decide
        // how long we should wait on the poll phase.
        let timeout = match self.timer_queue.iter().next() {
            Some((t, _)) => *t - Instant::now(),
            None if self.pending_tasks > 0 => Duration::MAX,
            None => Duration::ZERO,
        };

        if let Ok(event) = self.event_queue.recv_timeout(timeout) {
            match event {
                Event::ThreadPool(index, result) => self.run_task_callback(index, result),
            }
            self.pending_tasks -= 1;
            self.prepare();
        }
    }

    fn run_task_callback(&mut self, index: Index, result: TaskResult) {
        if let Some(cb) = self.task_callbacks.remove(&index) {
            (cb)(result);
        }
    }

    fn ev_new_timer(&mut self, index: Index, delay: Duration, cb: Box<dyn FnOnce()>) {
        let time_key = Instant::now() + delay;
        self.timer_callbacks.insert(index, cb);
        self.timer_queue.insert(time_key, index);
    }

    fn ev_remove_timer(&mut self, index: &Index) {
        self.timer_callbacks.remove(index);
        self.timer_queue.retain(|_, v| *v != *index);
    }

    fn ev_spawn_task(
        &mut self,
        index: Index,
        task: Box<dyn FnOnce() -> TaskResult + Send>,
        task_cb: Option<Box<dyn FnOnce(TaskResult)>>,
    ) {
        let notifier = self.event_dispatcher.clone();

        if let Some(cb) = task_cb {
            self.task_callbacks.insert(index, cb);
        }

        self.thread_pool.execute(move || {
            let result = (task)();
            let notifier = notifier.lock().unwrap();

            notifier.send(Event::ThreadPool(index, result)).unwrap();
        });

        self.pending_tasks += 1;
    }
}

impl Default for EventLoop {
    fn default() -> Self {
        Self::new()
    }
}

pub struct LoopHandle {
    index: Rc<Cell<Index>>,
    dispatcher: Rc<mpsc::Sender<Action>>,
    actions_queue_empty: Rc<Cell<bool>>,
}

#[allow(dead_code)]
impl LoopHandle {
    pub fn index(&self) -> Index {
        let index = self.index.get();
        self.index.set(index + 1);
        index
    }

    pub fn timer<F>(&self, delay: u64, cb: F) -> Index
    where
        F: FnOnce() + 'static,
    {
        let index = self.index();
        let expires_at = Duration::from_millis(delay);

        self.dispatcher
            .send(Action::NewTimer(index, expires_at, Box::new(cb)))
            .unwrap();

        self.actions_queue_empty.set(false);

        index
    }

    pub fn remove_timer(&self, index: &Index) {
        self.dispatcher.send(Action::RemoveTimer(*index)).unwrap();
        self.actions_queue_empty.set(false);
    }

    pub fn spawn<F, U>(&self, task: F, task_cb: Option<U>) -> Index
    where
        F: FnOnce() -> TaskResult + Send + 'static,
        U: FnOnce(TaskResult) + 'static,
    {
        let index = self.index();

        // Note: I tried to use `.and_then` instead of this ugly match statement but Rust complains
        // about mismatch types having no idea why.
        let task_cb: Option<Box<dyn FnOnce(TaskResult)>> = match task_cb {
            Some(cb) => Some(Box::new(cb)),
            None => None,
        };

        self.dispatcher
            .send(Action::SpawnTask(index, Box::new(task), task_cb))
            .unwrap();

        self.actions_queue_empty.set(false);

        index
    }
}

fn main() {
    let mut event_loop = EventLoop::new();
    let handle = event_loop.handle();

    handle.spawn(
        || {
            std::thread::sleep(Duration::from_millis(2500));
            println!("Hello, world!");
            None
        },
        None::<fn(_)>,
    );

    handle.timer(1000, || println!("Hello!"));

    while event_loop.has_pending_events() {
        event_loop.tick();
    }
}
