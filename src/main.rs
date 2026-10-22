use std::net::SocketAddr;
use uring_http::server::demo_routes;
use uring_http::{Config, Server};

fn main() {
    let addr: SocketAddr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8080".into())
        .parse()
        .expect("usage: uring-http [ADDR:PORT]");
    let mut srv = Server::bind(addr, Config::default(), demo_routes).unwrap_or_else(|e| {
        eprintln!("bind/io_uring setup failed: {e}");
        std::process::exit(1);
    });
    eprintln!("listening on http://{}", srv.local_addr().unwrap());
    if let Err(e) = srv.run() {
        eprintln!("event loop error: {e}");
        std::process::exit(1);
    }
}
