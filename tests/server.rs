use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use uring_http::server::demo_routes;
use uring_http::{Config, Limits, Server, ShutdownHandle, Stats};

struct Running {
    addr: SocketAddr,
    stop: ShutdownHandle,
    stats: Arc<Stats>,
    join: Option<JoinHandle<std::io::Result<()>>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.shutdown();
        if let Some(j) = self.join.take() {
            j.join().unwrap().unwrap();
        }
    }
}

fn start_with(cfg: Config) -> Running {
    let mut srv = Server::bind("127.0.0.1:0".parse().unwrap(), cfg, demo_routes).unwrap();
    let addr = srv.local_addr().unwrap();
    let stop = srv.shutdown_handle();
    let stats = srv.stats();
    let join = thread::spawn(move || srv.run());
    Running {
        addr,
        stop,
        stats,
        join: Some(join),
    }
}

fn start() -> Running {
    start_with(Config::default())
}

fn connect(addr: SocketAddr) -> TcpStream {
    let s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.set_nodelay(true).unwrap();
    s
}

/// Read one response: (status, headers, body).
fn read_response<R: BufRead>(r: &mut R) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    assert!(line.starts_with("HTTP/1.1 "), "bad status line {line:?}");
    let status: u16 = line[9..12].parse().unwrap();
    let mut headers = Vec::new();
    let mut len = 0usize;
    loop {
        line.clear();
        r.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        let (k, v) = line.trim_end().split_once(": ").unwrap();
        if k.eq_ignore_ascii_case("content-length") {
            len = v.parse().unwrap();
        }
        headers.push((k.to_string(), v.to_string()));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).unwrap();
    (status, headers, body)
}

fn hdr<'a>(h: &'a [(String, String)], k: &str) -> Option<&'a str> {
    h.iter()
        .find(|(a, _)| a.eq_ignore_ascii_case(k))
        .map(|(_, v)| v.as_str())
}

fn assert_eof<R: Read>(r: &mut R) {
    let mut b = [0u8; 1];
    assert_eq!(r.read(&mut b).unwrap(), 0, "expected server to close");
}

#[test]
fn get_root() {
    let s = start();
    let mut c = connect(s.addr);
    c.write_all(b"GET / HTTP/1.1\r\nHost: t\r\n\r\n").unwrap();
    let (st, h, b) = read_response(&mut BufReader::new(&c));
    assert_eq!(st, 200);
    assert_eq!(b, b"hello from io_uring\n");
    assert_eq!(hdr(&h, "connection"), Some("keep-alive"));
}

#[test]
fn post_echo_large_body() {
    let s = start();
    let c = connect(s.addr);
    let body: Vec<u8> = (0..200_000u32).map(|i| (i * 31 % 251) as u8).collect();
    let mut w = c.try_clone().unwrap();
    let body2 = body.clone();
    let writer = thread::spawn(move || {
        write!(
            w,
            "POST /echo HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            body2.len()
        )
        .unwrap();
        w.write_all(&body2).unwrap();
    });
    let (st, _, b) = read_response(&mut BufReader::new(&c));
    writer.join().unwrap();
    assert_eq!(st, 200);
    assert_eq!(b, body);
}

#[test]
fn keep_alive_many_requests_one_connection() {
    let s = start();
    let c = connect(s.addr);
    let mut r = BufReader::new(c.try_clone().unwrap());
    let mut w = c;
    for i in 0..500 {
        let payload = format!("n={i}");
        write!(
            w,
            "POST /echo HTTP/1.1\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        )
        .unwrap();
        let (st, _, b) = read_response(&mut r);
        assert_eq!(st, 200);
        assert_eq!(b, payload.as_bytes());
    }
    assert_eq!(s.stats.accepted.load(Ordering::Relaxed), 1);
}

#[test]
fn pipelined_requests_answered_in_order() {
    let s = start();
    let mut c = connect(s.addr);
    let mut batch = Vec::new();
    for i in 0..50 {
        let p = format!("{i}");
        batch.extend_from_slice(
            format!(
                "POST /echo HTTP/1.1\r\nContent-Length: {}\r\n\r\n{p}",
                p.len()
            )
            .as_bytes(),
        );
    }
    batch.extend_from_slice(b"GET /missing HTTP/1.1\r\nConnection: close\r\n\r\n");
    c.write_all(&batch).unwrap();
    let mut r = BufReader::new(&c);
    for i in 0..50 {
        let (st, _, b) = read_response(&mut r);
        assert_eq!(st, 200);
        assert_eq!(b, format!("{i}").as_bytes());
    }
    let (st, h, _) = read_response(&mut r);
    assert_eq!(st, 404);
    assert_eq!(hdr(&h, "connection"), Some("close"));
    assert_eof(&mut r);
}

#[test]
fn byte_at_a_time() {
    let s = start();
    let mut c = connect(s.addr);
    c.set_nodelay(true).unwrap();
    for b in b"POST /echo HTTP/1.1\r\nContent-Length: 3\r\n\r\nxyz" {
        c.write_all(&[*b]).unwrap();
        thread::sleep(Duration::from_micros(200));
    }
    let (st, _, b) = read_response(&mut BufReader::new(&c));
    assert_eq!(st, 200);
    assert_eq!(b, b"xyz");
}

#[test]
fn http10_closes_by_default_and_head_has_no_body() {
    let s = start();
    let mut c = connect(s.addr);
    c.write_all(b"HEAD / HTTP/1.0\r\n\r\n").unwrap();
    let mut r = BufReader::new(&c);
    let mut line = String::new();
    let mut raw = String::new();
    while r.read_line(&mut line).unwrap() > 0 {
        raw.push_str(&line);
        line.clear();
    }
    assert!(raw.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(raw.contains("Content-Length: 20\r\n"));
    assert!(raw.contains("Connection: close\r\n"));
    assert!(
        raw.ends_with("\r\n\r\n"),
        "HEAD must not carry a body: {raw:?}"
    );
}

#[test]
fn malformed_and_unsupported_get_error_and_close() {
    let s = start();
    for (req, code) in [
        (&b"BROKEN\r\n\r\n"[..], 400u16),
        (b"GET / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n", 501),
        (b"GET / HTTP/3.0\r\n\r\n", 505),
        (b"DELETE /echo HTTP/1.1\r\n\r\n", 405),
    ] {
        let mut c = connect(s.addr);
        c.write_all(req).unwrap();
        let mut r = BufReader::new(&c);
        let (st, _, _) = read_response(&mut r);
        assert_eq!(st, code, "for {:?}", String::from_utf8_lossy(req));
        if code != 405 {
            assert_eof(&mut r);
        }
    }
    assert_eq!(s.stats.rejected.load(Ordering::Relaxed), 3);
}

#[test]
fn limits_enforced() {
    let cfg = Config {
        limits: Limits {
            max_head: 256,
            max_body: 16,
            max_headers: 8,
        },
        ..Config::default()
    };
    let s = start_with(cfg);
    let mut c = connect(s.addr);
    c.write_all(b"POST /echo HTTP/1.1\r\nContent-Length: 17\r\n\r\n")
        .unwrap();
    let (st, _, _) = read_response(&mut BufReader::new(&c));
    assert_eq!(st, 413);

    let mut c = connect(s.addr);
    let mut big = b"GET / HTTP/1.1\r\nX: ".to_vec();
    big.extend(std::iter::repeat(b'a').take(400));
    c.write_all(&big).unwrap();
    let (st, _, _) = read_response(&mut BufReader::new(&c));
    assert_eq!(st, 431);
}

#[test]
fn many_concurrent_clients() {
    let s = start();
    let addr = s.addr;
    let workers: Vec<_> = (0..64)
        .map(|t| {
            thread::spawn(move || {
                let c = connect(addr);
                let mut r = BufReader::new(c.try_clone().unwrap());
                let mut w = c;
                for i in 0..50 {
                    let p = format!("t{t}-{i}");
                    write!(
                        w,
                        "POST /echo HTTP/1.1\r\nContent-Length: {}\r\n\r\n{p}",
                        p.len()
                    )
                    .unwrap();
                    let (st, _, b) = read_response(&mut r);
                    assert_eq!(st, 200);
                    assert_eq!(b, p.as_bytes());
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    assert_eq!(s.stats.requests.load(Ordering::Relaxed), 64 * 50);
}

#[test]
fn max_connections_rejects_extra_and_slots_are_reused() {
    let cfg = Config {
        max_connections: 2,
        ..Config::default()
    };
    let s = start_with(cfg);
    let a = connect(s.addr);
    let b = connect(s.addr);
    // Make sure both were accepted before the third.
    for mut c in [&a, &b] {
        c.write_all(b"GET /health HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(read_response(&mut BufReader::new(c)).0, 200);
    }
    let mut extra = connect(s.addr);
    let _ = extra.write_all(b"GET / HTTP/1.1\r\n\r\n");
    let mut buf = [0u8; 16];
    assert!(
        matches!(extra.read(&mut buf), Ok(0) | Err(_)),
        "third connection must be dropped"
    );
    drop(a);
    drop(b);
    // Wait for the server to observe the closes, then a new client must work.
    for _ in 0..200 {
        if s.stats.closed.load(Ordering::Relaxed) >= 3 {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let mut c = connect(s.addr);
    c.write_all(b"GET /health HTTP/1.1\r\n\r\n").unwrap();
    assert_eq!(read_response(&mut BufReader::new(&c)).0, 200);
}

#[test]
fn shutdown_with_idle_connections_open() {
    let s = start();
    let _idle: Vec<_> = (0..10).map(|_| connect(s.addr)).collect();
    thread::sleep(Duration::from_millis(50));
    drop(s); // Drop joins the server thread; must not hang.
}
