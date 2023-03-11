//! The main event loop which performs I/O on the pseudoterminal.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};
use std::marker::Send;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

use log::error;

use smol::channel::{self, Receiver, Sender};
use smol::fs::File;
use smol::lock::Mutex;
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
        EventLoop {
            pty,
            tx,
            rx,
            terminal,
            event_proxy,
            hold,
            ref_test,
        }
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
        state: &Mutex<State>,
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
                let mut state = state.lock().await;
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
        if state.lock().await.parser.sync_bytes_count() < processed && processed > 0 {
            event_proxy.send_event(Event::Wakeup);
        }

        Ok(())
    }

    #[inline]
    async fn pty_write(pty: &T, state: &Mutex<State>) -> io::Result<()> {
        let mut state_lock = state.lock().await;
        state_lock.ensure_next();
        let mut state_lock = Some(state_lock);

        'write_many: while let Some(mut current) = state_lock.as_mut().unwrap().take_current() {
            'write_one: loop {
                match pty.write(current.remaining_bytes()).await {
                    Ok(0) => {
                        state.set_current(Some(current));
                        break 'write_many;
                    },
                    Ok(n) => {
                        current.advance(n);
                        if current.finished() {
                            state.goto_next();
                            break 'write_one;
                        }
                    },
                    Err(err) => {
                        state.set_current(Some(current));
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
            let mut state = Mutex::new(State::default());
            let mut buf = [0u8; READ_BUFFER_SIZE];

            let mut tokens = (0..).map(Into::into);

            let poll_opts = PollOpt::edge() | PollOpt::oneshot();

            let channel_token = tokens.next().unwrap();
            self.poll.register(&self.rx, channel_token, Ready::readable(), poll_opts).unwrap();

            // Register TTY through EventedRW interface.
            self.pty.register(&self.poll, &mut tokens, Ready::readable(), poll_opts).unwrap();

            let mut events = Events::with_capacity(1024);

            // Split `self` up into mutable-reference parts for tasks.
            let Self { executor, pty, rx, tx, terminal, event_proxy, hold, ref_test } = &mut self;

            // Start blocking and running the executor.
            let event_proxy = self.event_proxy;
            smol::block_on(executor.run(async move {

                let mut pipe = if self.ref_test {
                    Some(File::create("./alacritty.recording").await.expect("create alacritty recording"))
                } else {
                    None
                };

                // Spawn tasks that polls the PTY.
                let pty = &*pty;
                let event_proxy = &*event_proxy;

                let read_from_pty = executor.spawn({
                    async move {
                        loop {
                            if let Err(e) = Self::pty_read(pty, terminal, event_proxy, &state, &mut buf, pipe.as_mut()).await {
                                error!("Error reading from pty: {}", e);
                                break;
                            }
                        }
                    }
                });

                let write_to_pty = executor.spawn({
                    async move {
                        loop {
                            if let Err(e) = Self::pty_write(pty, &state).await {
                                error!("Error writing to pty: {}", e);
                                break;
                            }
                        }
                    }
                });

                loop {
                    // Begin synchronizing the terminal.
                    let sync_timeout = state.lock().await.parser.sync_timeout();

                    // If the timeout is reached before we receive any events, we need to wake up
                    // the window.
                    let run_timeout = async move {
                        sync_timeout.map_or_else(|| smol::Timer::never(), |timeout| smol::Timer::at(*timeout))
                            .await;

                        state.lock().await.parser.stop_sync(&mut *terminal.lock());
                        event_proxy.send_event(Event::Wakeup);
                        true
                    };

                    // Drain channel events until the channel wants to shutdown.
                    let drain_channel = async move {
                        // Wait for a new channel message.
                        let msg = self.rx.recv().await.expect("Channel should never be closed");

                        // Process the message.
                        let mut state = state.lock().await;
                        let mut keep_going = self.handle_message(&mut state, msg);

                        // Drain the remaining messages.
                        if keep_going {
                            keep_going = self.drain_recv_channel(&mut state);
                        }

                        keep_going
                    };

                    let keep_going = run_timeout.or(drain_channel).await;

                    if !keep_going {
                        break;
                    }

                    // Make sure we're not blocking the terminal too long unnecessarily.
                    smol::future::yield_now().await;
                }

                // Cancel outstanding tasks.
                drop(read_from_pty);
            }));

            'event_loop: loop {
                // Wakeup the event loop when a synchronized update timeout was reached.
                let sync_timeout = state.parser.sync_timeout();
                let timeout = sync_timeout.map(|st| st.saturating_duration_since(Instant::now()));

                // Handle synchronized update timeout.
                if events.is_empty() {

                    continue;
                }

                for event in events.iter() {
                    match event.token() {
                        token if token == channel_token => {
                            if !self.channel_event(channel_token, &mut state) {
                                break 'event_loop;
                            }
                        },

                        token if token == self.pty.child_event_token() => {
                            if let Some(tty::ChildEvent::Exited) = self.pty.next_child_event() {
                                if self.hold {
                                    // With hold enabled, make sure the PTY is drained.
                                    let _ = self.pty_read(&mut state, &mut buf, pipe.as_mut());
                                } else {
                                    // Without hold, shutdown the terminal.
                                    self.terminal.lock().exit();
                                }

                                self.event_proxy.send_event(Event::Wakeup);
                                break 'event_loop;
                            }
                        },

                        token
                            if token == self.pty.read_token()
                                || token == self.pty.write_token() =>
                        {
                            #[cfg(unix)]
                            if UnixReady::from(event.readiness()).is_hup() {
                                // Don't try to do I/O on a dead PTY.
                                continue;
                            }

                            if event.readiness().is_readable() {
                                if let Err(err) = self.pty_read(&mut state, &mut buf, pipe.as_mut())
                                {
                                    // On Linux, a `read` on the master side of a PTY can fail
                                    // with `EIO` if the client side hangs up.  In that case,
                                    // just loop back round for the inevitable `Exited` event.
                                    // This sucks, but checking the process is either racy or
                                    // blocking.
                                    #[cfg(target_os = "linux")]
                                    if err.raw_os_error() == Some(libc::EIO) {
                                        continue;
                                    }

                                    error!("Error reading from PTY in event loop: {}", err);
                                    break 'event_loop;
                                }
                            }

                            if event.readiness().is_writable() {
                                if let Err(err) = self.pty_write(&mut state) {
                                    error!("Error writing to PTY in event loop: {}", err);
                                    break 'event_loop;
                                }
                            }
                        },
                        _ => (),
                    }
                }

                // Register write interest if necessary.
                let mut interest = Ready::readable();
                if state.needs_write() {
                    interest.insert(Ready::writable());
                }
                // Reregister with new interest.
                self.pty.reregister(&self.poll, interest, poll_opts).unwrap();
            }

            // The evented instances are not dropped here so deregister them explicitly.
            let _ = self.poll.deregister(&self.rx);
            let _ = self.pty.deregister(&self.poll);

            (self, state)
        })
    }
}
