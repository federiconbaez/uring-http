//! Closed-loop keep-alive load generator.
//!
//! usage: cargo run --release --example loadgen -- [connections] [seconds] [addr]
//! With no addr it starts an in-process server on 127.0.0.1:0.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let conns: usize = args.get(1).map(|s| s.parse().unwrap()).unwrap_or(32);
    let secs: u64 = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(5);
    let addr: SocketAddr = match args.get(3) {
        Some(a) => a.parse().unwrap(),
        None => {
            let srv = uring_http::Server::bind(
                "127.0.0.1:0".parse().unwrap(),
                uring_http::Config::default(),
                uring_http::server::demo_routes,
            )
            .unwrap();
            let a = srv.local_addr().unwrap();
            std::thread::spawn(move || {
                let mut s = srv;
                s.run().unwrap()
            });
            a
        }
    };

    const REQ: &[u8] = b"GET /health HTTP/1.1\r\nHost: bench\r\n\r\n";
    let stop = Arc::new(AtomicBool::new(false));
    let total = Arc::new(AtomicU64::new(0));
    let workers: Vec<_> = (0..conns)
        .map(|_| {
            let stop = stop.clone();
            let total = total.clone();
            std::thread::spawn(move || {
                let mut s = TcpStream::connect(addr).unwrap();
                s.set_nodelay(true).unwrap();
                let mut buf = [0u8; 4096];
                let mut lat = Vec::with_capacity(1 << 16);
                while !stop.load(Ordering::Relaxed) {
                    let t = Instant::now();
                    s.write_all(REQ).unwrap();
                    // Response for /health is fixed-size; read until we have it all.
                    let mut got = 0;
                    loop {
                        let n = s.read(&mut buf[got..]).unwrap();
                        assert!(n > 0);
                        got += n;
                        if buf[..got].ends_with(b"ok\n") {
                            break;
                        }
                    }
                    lat.push(t.elapsed().as_nanos() as u64);
                    total.fetch_add(1, Ordering::Relaxed);
                }
                lat
            })
        })
        .collect();
    std::thread::sleep(Duration::from_secs(secs));
    stop.store(true, Ordering::Relaxed);
    let mut lat: Vec<u64> = workers
        .into_iter()
        .flat_map(|w| w.join().unwrap())
        .collect();
    lat.sort_unstable();
    let n = total.load(Ordering::Relaxed);
    let pct = |p: f64| lat[((lat.len() as f64 - 1.0) * p) as usize] as f64 / 1000.0;
    println!(
        "connections={conns} seconds={secs} requests={n} req_per_s={:.0} p50_us={:.1} p99_us={:.1}",
        n as f64 / secs as f64,
        pct(0.50),
        pct(0.99)
    );
}
