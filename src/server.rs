//! Single-threaded `io_uring` event loop.
//!
//! Each connection has at most one operation in flight (recv *or* send), so
//! its buffers are never moved or mutated while the kernel holds a pointer to
//! them. The listener always has exactly one `Accept` armed.

use crate::http::{parse_request, Limits, Request, Response};
use io_uring::{opcode, squeue, types, IoUring};
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const RECV_CHUNK: usize = 16 * 1024;

const OP_ACCEPT: u64 = 1;
const OP_RECV: u64 = 2;
const OP_SEND: u64 = 3;
const OP_CLOSE: u64 = 4;
const OP_WAKE: u64 = 5;

fn ud(op: u64, idx: usize) -> u64 {
    (op << 32) | idx as u64
}

fn split_ud(v: u64) -> (u64, usize) {
    (v >> 32, (v & 0xffff_ffff) as usize)
}

/// Server configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub ring_entries: u32,
    pub max_connections: usize,
    pub limits: Limits,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ring_entries: 256,
            max_connections: 1024,
            limits: Limits::default(),
        }
    }
}

/// Counters exposed for tests and the `/stats` route.
#[derive(Debug, Default)]
pub struct Stats {
    pub accepted: AtomicU64,
    pub requests: AtomicU64,
    pub rejected: AtomicU64,
    pub closed: AtomicU64,
}

/// Wakes the event loop through an `eventfd` and asks it to stop.
#[derive(Clone)]
pub struct ShutdownHandle {
    efd: Arc<OwnedFd>,
}

impl ShutdownHandle {
    pub fn shutdown(&self) {
        let one: u64 = 1;
        // SAFETY: writing 8 bytes from a valid u64 into an eventfd we own.
        unsafe {
            libc::write(
                self.efd.as_raw_fd(),
                &one as *const u64 as *const libc::c_void,
                8,
            );
        }
    }
}

struct Conn {
    fd: RawFd,
    recv_buf: Box<[u8; RECV_CHUNK]>,
    inbuf: Vec<u8>,
    outbuf: Vec<u8>,
    sent: usize,
    close_after_send: bool,
    closing: bool,
}

/// An HTTP server bound to a TCP address.
pub struct Server<H> {
    listener: TcpListener,
    ring: IoUring,
    cfg: Config,
    handler: H,
    conns: Vec<Option<Conn>>,
    free: Vec<usize>,
    efd: Arc<OwnedFd>,
    wake_buf: Box<[u8; 8]>,
    inflight: usize,
    stats: Arc<Stats>,
    stopping: bool,
}

impl<H: FnMut(&Request) -> Response> Server<H> {
    pub fn bind(addr: SocketAddr, cfg: Config, handler: H) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let ring = IoUring::new(cfg.ring_entries)?;
        // SAFETY: plain syscall; result checked below.
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh fd we own.
        let efd = Arc::new(unsafe { OwnedFd::from_raw_fd(raw) });
        Ok(Self {
            listener,
            ring,
            cfg,
            handler,
            conns: Vec::new(),
            free: Vec::new(),
            efd,
            wake_buf: Box::new([0; 8]),
            inflight: 0,
            stats: Arc::new(Stats::default()),
            stopping: false,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            efd: self.efd.clone(),
        }
    }

    pub fn stats(&self) -> Arc<Stats> {
        self.stats.clone()
    }

    fn push(&mut self, e: squeue::Entry) -> io::Result<()> {
        loop {
            // SAFETY: every buffer referenced by `e` lives in `self` (boxed or
            // in a Vec that is not touched until the completion arrives).
            let pushed = unsafe { self.ring.submission().push(&e).is_ok() };
            if pushed {
                self.inflight += 1;
                return Ok(());
            }
            self.ring.submit()?;
        }
    }

    fn arm_accept(&mut self) -> io::Result<()> {
        let e = opcode::Accept::new(
            types::Fd(self.listener.as_raw_fd()),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
        .flags(libc::SOCK_CLOEXEC)
        .build()
        .user_data(ud(OP_ACCEPT, 0));
        self.push(e)
    }

    fn arm_wake(&mut self) -> io::Result<()> {
        let e = opcode::Read::new(
            types::Fd(self.efd.as_raw_fd()),
            self.wake_buf.as_mut_ptr(),
            8,
        )
        .build()
        .user_data(ud(OP_WAKE, 0));
        self.push(e)
    }

    fn arm_recv(&mut self, idx: usize) -> io::Result<()> {
        let c = self.conns[idx].as_mut().expect("live conn");
        let e = opcode::Recv::new(types::Fd(c.fd), c.recv_buf.as_mut_ptr(), RECV_CHUNK as u32)
            .build()
            .user_data(ud(OP_RECV, idx));
        self.push(e)
    }

    fn arm_send(&mut self, idx: usize) -> io::Result<()> {
        let c = self.conns[idx].as_mut().expect("live conn");
        let rest = &c.outbuf[c.sent..];
        let e = opcode::Send::new(types::Fd(c.fd), rest.as_ptr(), rest.len() as u32)
            .flags(libc::MSG_NOSIGNAL)
            .build()
            .user_data(ud(OP_SEND, idx));
        self.push(e)
    }

    fn arm_close(&mut self, idx: usize) -> io::Result<()> {
        let c = self.conns[idx].as_mut().expect("live conn");
        c.closing = true;
        let e = opcode::Close::new(types::Fd(c.fd))
            .build()
            .user_data(ud(OP_CLOSE, idx));
        self.push(e)
    }

    /// Parse every complete request in `inbuf`, append responses to `outbuf`.
    fn process(&mut self, idx: usize) {
        let limits = self.cfg.limits;
        let c = self.conns[idx].as_mut().expect("live conn");
        let mut consumed = 0;
        loop {
            match parse_request(&c.inbuf[consumed..], &limits) {
                Ok(Some((req, n))) => {
                    consumed += n;
                    self.stats.requests.fetch_add(1, Ordering::Relaxed);
                    let keep = req.keep_alive();
                    let resp = (self.handler)(&req);
                    resp.write_to(&mut c.outbuf, keep, req.method == "HEAD");
                    if !keep {
                        c.close_after_send = true;
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                    Response::new(e.status(), format!("{}\n", crate::http::reason(e.status())))
                        .write_to(&mut c.outbuf, false, false);
                    c.close_after_send = true;
                    break;
                }
            }
        }
        c.inbuf.drain(..consumed);
    }

    /// After a recv or a completed send: send pending output, close, or read more.
    fn advance(&mut self, idx: usize) -> io::Result<()> {
        let c = self.conns[idx].as_mut().expect("live conn");
        if c.sent < c.outbuf.len() {
            return self.arm_send(idx);
        }
        c.outbuf.clear();
        c.sent = 0;
        if c.close_after_send || self.stopping {
            return self.arm_close(idx);
        }
        self.arm_recv(idx)
    }

    fn on_accept(&mut self, res: i32) -> io::Result<()> {
        if res >= 0 {
            self.stats.accepted.fetch_add(1, Ordering::Relaxed);
            let live = self.conns.len() - self.free.len();
            if live >= self.cfg.max_connections || self.stopping {
                // SAFETY: fd returned by accept; we own it.
                unsafe { libc::close(res) };
            } else {
                let conn = Conn {
                    fd: res,
                    recv_buf: Box::new([0; RECV_CHUNK]),
                    inbuf: Vec::new(),
                    outbuf: Vec::new(),
                    sent: 0,
                    close_after_send: false,
                    closing: false,
                };
                let idx = match self.free.pop() {
                    Some(i) => {
                        self.conns[i] = Some(conn);
                        i
                    }
                    None => {
                        self.conns.push(Some(conn));
                        self.conns.len() - 1
                    }
                };
                self.arm_recv(idx)?;
            }
        }
        if !self.stopping {
            self.arm_accept()?;
        }
        Ok(())
    }

    fn on_recv(&mut self, idx: usize, res: i32) -> io::Result<()> {
        if res <= 0 {
            return self.arm_close(idx);
        }
        let c = self.conns[idx].as_mut().expect("live conn");
        c.inbuf.extend_from_slice(&c.recv_buf[..res as usize]);
        self.process(idx);
        self.advance(idx)
    }

    fn on_send(&mut self, idx: usize, res: i32) -> io::Result<()> {
        if res < 0 {
            return self.arm_close(idx);
        }
        let c = self.conns[idx].as_mut().expect("live conn");
        c.sent += res as usize;
        self.advance(idx)
    }

    fn on_close(&mut self, idx: usize) {
        self.conns[idx] = None;
        self.free.push(idx);
        self.stats.closed.fetch_add(1, Ordering::Relaxed);
    }

    fn begin_stop(&mut self) {
        self.stopping = true;
        // Wake any pending accept/recv so every op completes and buffers can be
        // freed safely.
        // SAFETY: shutdown(2) on fds we own; harmless if already closed by peer.
        unsafe {
            libc::shutdown(self.listener.as_raw_fd(), libc::SHUT_RDWR);
            for c in self.conns.iter().flatten() {
                if !c.closing {
                    libc::shutdown(c.fd, libc::SHUT_RDWR);
                }
            }
        }
    }

    /// Run until [`ShutdownHandle::shutdown`] is called.
    pub fn run(&mut self) -> io::Result<()> {
        self.arm_accept()?;
        self.arm_wake()?;
        loop {
            if self.stopping && self.inflight == 0 {
                return Ok(());
            }
            match self.ring.submit_and_wait(1) {
                Ok(_) => {}
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
                Err(e) => return Err(e),
            }
            let cqes: Vec<(u64, i32)> = self
                .ring
                .completion()
                .map(|c| (c.user_data(), c.result()))
                .collect();
            for (data, res) in cqes {
                self.inflight -= 1;
                let (op, idx) = split_ud(data);
                match op {
                    OP_ACCEPT => self.on_accept(res)?,
                    OP_RECV => self.on_recv(idx, res)?,
                    OP_SEND => self.on_send(idx, res)?,
                    OP_CLOSE => self.on_close(idx),
                    OP_WAKE => self.begin_stop(),
                    _ => unreachable!("unknown op {op}"),
                }
            }
        }
    }
}

/// The demo routing table used by the binary and the integration tests.
pub fn demo_routes(req: &Request) -> Response {
    let path = req.target.split('?').next().unwrap_or("");
    match (req.method.as_str(), path) {
        ("GET" | "HEAD", "/") => {
            Response::new(200, "hello from io_uring\n").with_header("Content-Type", "text/plain")
        }
        ("GET" | "HEAD", "/health") => Response::new(200, "ok\n"),
        ("POST", "/echo") => Response::new(200, req.body.clone())
            .with_header("Content-Type", "application/octet-stream"),
        (_, "/" | "/health" | "/echo") => Response::new(405, "method not allowed\n"),
        _ => Response::new(404, "not found\n"),
    }
}
