use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use uring_http::http::{parse_request, Limits};

fn bench(c: &mut Criterion) {
    let lim = Limits::default();
    let small =
        b"GET /health HTTP/1.1\r\nHost: localhost\r\nUser-Agent: bench\r\nAccept: */*\r\n\r\n";
    let mut post =
        b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1024\r\n\r\n".to_vec();
    post.extend(std::iter::repeat(b'x').take(1024));
    let mut g = c.benchmark_group("parse_request");
    g.throughput(Throughput::Bytes(small.len() as u64));
    g.bench_function("get_small", |b| {
        b.iter(|| parse_request(black_box(small), &lim).unwrap())
    });
    g.throughput(Throughput::Bytes(post.len() as u64));
    g.bench_function("post_1k", |b| {
        b.iter(|| parse_request(black_box(&post), &lim).unwrap())
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
