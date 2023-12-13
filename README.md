# Dune's Event Loop Library

This library is a multi-platform support library with a focus on asynchronous I/O. It was primarily developed for use by [Dune](https://github.com/aalykiot/dune), but can be also used in any Rust project.

![GitHub](https://img.shields.io/github/license/aalykiot/dune-event-loop?style=flat-square)
![GitHub Workflow Status](https://img.shields.io/github/actions/workflow/status/aalykiot/dune-event-loop/ci.yml?branch=main&style=flat-square)

## Features

- Timers
- Asynchronous TCP sockets
- Thread pool
- File system events

## Documentation

**Timer** handles are used to schedule callbacks to be called in the future.

```rust
fn main() {
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();

    handle.timer(1000, false, |h: LoopHandle| {
        println!("Hello!");
        h.timer(2500, false, |_: LoopHandle| println!("Hello, world!"));
    });

    while event_loop.has_pending_events() {
        event_loop.tick();
    }
}
```

This library also provides a **thread-pool** which can be used to run user code and get notified in the loop thread. This thread pool is internally used by the Dune runtime to run all file system operations, as well as DNS lookups.

```rust
fn main() {
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();

    let read_file = || {
        let content = std::fs::read_to_string("./examples/async.rs").unwrap();
        Some(Ok(content.as_bytes().to_vec()))
    };

    let read_file_cb = |_: LoopHandle, result: TaskResult| {
        let bytes = result.unwrap().unwrap();
        let content = std::str::from_utf8(&bytes).unwrap();
        println!("{}", content);
    };

    handle.spawn(read_file, Some(read_file_cb));

    while event_loop.has_pending_events() {
        event_loop.tick();
    }
}
```

**TCP handles** are used to create TCP socket streams.

```rust
fn main() {
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();

    let on_close = |_: LoopHandle| println!("Connection closed.");

    let on_write = |_: LoopHandle, _: Index, result: Result<usize>| {
        if let Err(e) = result {
            eprintln!("{}", e);
        }
    };

    let on_read = move |h: LoopHandle, index: Index, data: Result<Vec<u8>>| {
        match data {
            Ok(data) if data.is_empty() => h.tcp_close(index, on_close),
            Ok(data) => h.tcp_write(index, &data, on_write),
            Err(e) => eprintln!("{}", e),
        };
    };

    let on_new_connection =
        move |h: LoopHandle, index: Index, socket: Result<TcpSocketInfo>| match socket {
            Ok(_) => h.tcp_read_start(index, on_read),
            Err(e) => eprintln!("{}", e),
        };

    match handle.tcp_listen("127.0.0.1:9000", on_new_connection) {
        Ok(_) => println!("Server is listening on 127.0.0.1:9000"),
        Err(e) => eprintln!("{}", e),
    };

    while event_loop.has_pending_events() {
        event_loop.tick();
    }
}
```

You can also connect to a remote host using a **TCP handle**.

```rust
handle
    .tcp_connect("104.21.45.178:80", on_connection)
    .unwrap();
```

**FS handles** are used to watch specified paths for changes.

```rust
fn main() {
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();

    let on_event = |_: LoopHandle, event: FsEvent| {
        println!("{event:?}");
    };

    let directory = "./examples/";
    let rid = match handle.fs_event_start(directory, true, on_event) {
        Ok(rid) => rid,
        Err(e) => {
            println!("{e}");
            return;
        }
    };

    handle.timer(10000, false, move |h: LoopHandle| h.fs_event_stop(&rid));

    while event_loop.has_pending_events() {
        event_loop.tick();
    }
}
```

> You can run all the above examples located in `/examples` folders using cargo: `cargo run --example [name]`
