//! The main event loop which performs I/O on the pseudoterminal.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};
use std::marker::Send;
use std::sync::Arc;
use std::thread::JoinHandle;

use log::error;

use smol::channel::{self, Receiver, Sender};
use smol::fs::File;
use smol::prelude::*;

use crate::event::{self, Event, EventListener, WindowSize};
use crate::sync::FairMutex;
use crate::term::Term;
use crate::{ansi, thread, tty};

/// Max bytes to read from the PTY before forced terminal synchronization.
const READ_BUFFER_SIZE: usize = 0x10_0000;

/// Max bytes to read from the PTY while the terminal is locked.
const MAX_LOCKED_READ: usize = u16::MAX as usize;

/// Messages that may be sent to the `EventLoop`.
#[derive(Debug)]
pub enum Msg {
    /// Data that should be written to the PTY.
    Input(Cow<'static, [u8]>),

    /// Indicates that the `EventLoop` should shut down, as Alacritty is shutting down.
    Shutdown,

    /// Instruction to resize the PTY.
    Resize(WindowSize),
}

/// The main event!.. loop.
///
/// Handles all the PTY I/O and runs the PTY parser which updates terminal
/// state.
pub struct EventLoop<T: tty::EventedPty, U: EventListener> {
    pty: T,
    rx: Receiver<Msg>,
    tx: Sender<Msg>,
    terminal: Arc<FairMutex<Term<U>>>,
    event_proxy: U,
    hold: bool,
    ref_test: bool,
}

/// Helper type which tracks how much of a buffer has been written.
struct Writing {
    source: Cow<'static, [u8]>,
    written: usize,
}

pub struct Notifier(pub Sender<Msg>);

impl event::Notify for Notifier {
    fn notify<B>(&self, bytes: B)
    where
        B: Into<Cow<'static, [u8]>>,
    {
        let bytes = bytes.into();
        // terminal hangs if we send 0 bytes through.
        if bytes.len() == 0 {
            return;
        }

        let _ = self.0.try_send(Msg::Input(bytes));
    }
}

impl event::OnResize for Notifier {
    fn on_resize(&mut self, window_size: WindowSize) {
        let _ = self.0.try_send(Msg::Resize(window_size));
    }
}

/// All of the mutable state needed to run the event loop.
///
/// Contains list of items to write, current write state, etc. Anything that
/// would otherwise be mutated on the `EventLoop` goes here.
#[derive(Default)]
pub struct State {
    write_list: VecDeque<Cow<'static, [u8]>>,
    writing: Option<Writing>,
    parser: ansi::Processor,
}

impl State {
    #[inline]
    fn ensure_next(&mut self) {
        if self.writing.is_none() {
            self.goto_next();
        }
    }

    #[inline]
    fn goto_next(&mut self) {
        self.writing = self.write_list.pop_front().map(Writing::new);
    }

    #[inline]
    fn take_current(&mut self) -> Option<Writing> {
        self.writing.take()
    }

    #[inline]
    fn needs_write(&self) -> bool {
        self.writing.is_some() || !self.write_list.is_empty()
    }

    #[inline]
    fn set_current(&mut self, new: Option<Writing>) {
        self.writing = new;
    }
}

impl Writing {
    #[inline]
    fn new(c: Cow<'static, [u8]>) -> Writing {
        Writing { source: c, written: 0 }
    }

    #[inline]
    fn advance(&mut self, n: usize) {
        self.written += n;
    }

    #[inline]
    fn remaining_bytes(&self) -> &[u8] {
        &self.source[self.written..]
    }

    #[inline]
    fn finished(&self) -> bool {
        self.written >= self.source.len()
    }
}

impl<T, U> EventLoop<T, U>
where
    T: tty::EventedPty + event::OnResize + Send + 'static,
    U: EventListener + Send + 'static,
{
    /// Create a new event loop.
    pub fn new(
        terminal: Arc<FairMutex<Term<U>>>,
        event_proxy: U,
        pty: T,
        hold: bool,
        ref_test: bool,
    ) -> EventLoop<T, U> {
        let (tx, rx) = channel::unbounded();
        EventLoop { pty, tx, rx, terminal, event_proxy, hold, ref_test }
    }

    pub fn channel(&self) -> Sender<Msg> {
        self.tx.clone()
    }

    /// Handle a channel message.
    ///
    /// Returns `false` when a shutdown message was received.
    fn handle_message(&mut self, state: &mut State, msg: Msg) -> bool {
        match msg {
            Msg::Input(input) => state.write_list.push_back(input),
            Msg::Resize(window_size) => self.pty.on_resize(window_size),
            Msg::Shutdown => return false,
        }

        true
    }

    /// Drain the channel.
    ///
    /// Returns `false` when a shutdown message was received.
    fn drain_recv_channel(&mut self, state: &mut State) -> bool {
        while let Ok(msg) = self.rx.try_recv() {
            if !self.handle_message(state, msg) {
                return false;
            }
        }

        true
    }

    #[inline]
    async fn pty_read<X>(
        pty: &T,
        terminal_lock: &FairMutex<Term<U>>,
        event_proxy: &U,
        state: &RefCell<State>,
        buf: &mut [u8],
        mut writer: Option<&mut X>,
    ) -> io::Result<()>
    where
        X: AsyncWrite,
    {
        let mut unprocessed = 0;
        let mut processed = 0;

        // Reserve the next terminal lock for PTY reading.
        let _terminal_lease = Some(terminal_lock.lease());
        let mut terminal = None;

        loop {
            // Read from the PTY.
            match pty.read(&mut buf[unprocessed..]).await {
                // This is received on Windows/macOS when no more data is readable from the PTY.
                Ok(0) if unprocessed == 0 => break,
                Ok(got) => unprocessed += got,
                Err(err) => match err.kind() {
                    ErrorKind::Interrupted | ErrorKind::WouldBlock => {
                        // Go back to mio if we're caught up on parsing and the PTY would block.
                        if unprocessed == 0 {
                            break;
                        }
                    },
                    _ => return Err(err),
                },
            }

            // Attempt to lock the terminal.
            let terminal = match &mut terminal {
                Some(terminal) => terminal,
                None => terminal.insert(match terminal_lock.try_lock_unfair() {
                    // Force block if we are at the buffer size limit.
                    None if unprocessed >= READ_BUFFER_SIZE => terminal_lock.lock_unfair(),
                    None => continue,
                    Some(terminal) => terminal,
                }),
            };

            // Write a copy of the bytes to the ref test file.
            if let Some(writer) = &mut writer {
                writer.write_all(&buf[..unprocessed]).await.unwrap();
            }

            // Parse the incoming bytes.
            {
                let mut state = state.borrow_mut();
                for byte in &buf[..unprocessed] {
                    state.parser.advance(&mut **terminal, *byte);
                }
            }

            processed += unprocessed;
            unprocessed = 0;

            // Assure we're not blocking the terminal too long unnecessarily.
            if processed >= MAX_LOCKED_READ {
                break;
            }
        }

        // Queue terminal redraw unless all processed bytes were synchronized.
        if state.borrow_mut().parser.sync_bytes_count() < processed && processed > 0 {
            event_proxy.send_event(Event::Wakeup);
        }

        Ok(())
    }

    #[inline]
    async fn pty_write(pty: &T, state: &RefCell<State>) -> io::Result<()> {
        'write_many: while let Some(mut current) = state.borrow_mut().take_current() {
            'write_one: loop {
                match pty.write(current.remaining_bytes()).await {
                    Ok(0) => {
                        state.borrow_mut().set_current(Some(current));
                        break 'write_many;
                    },
                    Ok(n) => {
                        current.advance(n);
                        if current.finished() {
                            state.borrow_mut().goto_next();
                            break 'write_one;
                        }
                    },
                    Err(err) => {
                        state.borrow_mut().set_current(Some(current));
                        match err.kind() {
                            ErrorKind::Interrupted | ErrorKind::WouldBlock => break 'write_many,
                            _ => return Err(err),
                        }
                    },
                }
            }
        }

        Ok(())
    }

    pub fn spawn(mut self) -> JoinHandle<(Self, State)> {
        thread::spawn_named("PTY reader", move || {
            let mut state = RefCell::new(State::default());
            let mut buf = [0u8; READ_BUFFER_SIZE];

            // Split `self` up into mutable-reference parts for tasks.
            let Self { pty, rx, tx, terminal, event_proxy, hold, ref_test } = &mut self;

            // Start blocking and running the executor.
            let executor = smol::LocalExecutor::new();
            let (shutdown, signal) = smol::channel::bounded::<()>(1);
            smol::block_on(executor.run(async move {
                let mut pipe = if self.ref_test {
                    Some(
                        File::create("./alacritty.recording")
                            .await
                            .expect("create alacritty recording"),
                    )
                } else {
                    None
                };

                // Spawn tasks that polls the PTY.
                let pty = &*pty;
                let event_proxy = &*event_proxy;

                // Read from the PTY.
                let read_from_pty = executor.spawn({
                    let shutdown = shutdown.clone();
                    async move {
                        loop {
                            if let Err(e) = Self::pty_read(
                                pty,
                                terminal,
                                event_proxy,
                                &state,
                                &mut buf,
                                pipe.as_mut(),
                            )
                            .await
                            {
                                error!("Error reading from pty: {}", e);
                                shutdown.send(()).await.ok();
                                break;
                            }
                        }
                    }
                });

                // Write to the PTY.
                let write_to_pty = executor.spawn({
                    async move {
                        loop {
                            if let Err(e) = Self::pty_write(pty, &state).await {
                                error!("Error writing to pty: {}", e);
                                shutdown.send(()).await.ok();
                                break;
                            }
                        }
                    }
                });

                // Handle child exit.
                let child_pty = executor.spawn({
                    async move {
                        loop {
                            if let tty::ChildEvent::Exited = pty.wait_child_event().await {
                                if self.hold {
                                    // With hold enabled, make sure the PTY is drained.
                                    let _ = Self::pty_read(
                                        pty,
                                        terminal,
                                        event_proxy,
                                        &state,
                                        &mut buf,
                                        pipe.as_mut(),
                                    )
                                    .await;
                                } else {
                                    // Without hold, shutdown the terminal.
                                    self.terminal.lock().exit();
                                }

                                self.event_proxy.send_event(Event::Wakeup);
                                shutdown.send(()).await.ok();
                                break;
                            }
                        }
                    }
                });

                loop {
                    // Begin synchronizing the terminal.
                    let sync_timeout = state.borrow_mut().parser.sync_timeout();

                    // If the timeout is reached before we receive any events, we need to wake up
                    // the window.
                    let run_timeout = async move {
                        sync_timeout
                            .map_or_else(
                                || smol::Timer::never(),
                                |timeout| smol::Timer::at(*timeout),
                            )
                            .await;

                        state.borrow_mut().parser.stop_sync(&mut *terminal.lock());
                        event_proxy.send_event(Event::Wakeup);
                        true
                    };

                    // Drain channel events until the channel wants to shutdown.
                    let drain_channel = async move {
                        // Wait for a new channel message.
                        let msg = self.rx.recv().await.expect("Channel should never be closed");

                        // Process the message.
                        let mut state = state.borrow_mut();
                        let mut keep_going = self.handle_message(&mut state, msg);

                        // Drain the remaining messages.
                        if keep_going {
                            keep_going = self.drain_recv_channel(&mut state);
                        }

                        keep_going
                    };

                    // If another task sends a shutdown signal, we should stop the event loop.
                    let shutdown = async move {
                        signal.recv().await;
                        false
                    };

                    let keep_going = shutdown.or(run_timeout).or(drain_channel).await;

                    if !keep_going {
                        break;
                    }

                    // Make sure we're not blocking the terminal too long unnecessarily.
                    smol::future::yield_now().await;
                }

                // Cancel outstanding tasks.
                read_from_pty.cancel().await;
                write_to_pty.cancel().await;
                child_pty.cancel().await;
            }));

            (self, state.into_inner())
        })
    }
}
