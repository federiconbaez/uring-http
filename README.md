# uring-http

Servidor HTTP/1.1 mínimo escrito en Rust directamente sobre `io_uring`, sin tokio ni ningún runtime async.
*A minimal HTTP/1.1 server in Rust driven directly by `io_uring`, with no tokio or other async runtime.*

[English below](#english)

## Español

### Qué es

Un solo hilo y un solo anillo `io_uring`. Las operaciones de socket (`accept`, `recv`, `send` y `close`) se envían al anillo y se resuelven desde la cola de completados. El parser HTTP también está escrito a mano en `src/http.rs`, sin `httparse`.

- HTTP/1.1 y HTTP/1.0: `GET`, `HEAD` y `POST` con `Content-Length`.
- Keep-alive (por defecto en 1.1; en 1.0 solo con `Connection: keep-alive`) y pipelining, con las respuestas en orden.
- Límites configurables: tamaño de cabecera (431), cuerpo (413) y cantidad de cabeceras.
- Rechazos explícitos: `Transfer-Encoding` → 501 (chunked **no** está implementado), versión desconocida → 505, sintaxis inválida, folding obsoleto o `Content-Length` duplicado/inválido → 400. En todos estos casos se cierra la conexión.
- Tope de conexiones simultáneas: las que exceden el tope se cierran apenas se aceptan.
- Apagado limpio con un `eventfd` registrado en el anillo. Antes de liberar los buffers, se espera a que terminen todas las operaciones en vuelo.

### Diseño de memoria

Cada conexión tiene **como máximo una operación en vuelo** (recv **o** send). Así, el buffer que el kernel está usando nunca se mueve ni se modifica mientras la operación no termina. El buffer de lectura es un `Box<[u8; 16384]>` con dirección fija. `user_data` codifica `(op << 32) | índice_de_slot`.

### Rutas de demo

| Ruta | Respuesta |
|---|---|
| `GET /` | `hello from io_uring` |
| `GET /health` | `ok` |
| `POST /echo` | devuelve el cuerpo |
| otro método en esas rutas | 405 |
| resto | 404 |

```sh
cargo run --release -- 127.0.0.1:8080
curl -i http://127.0.0.1:8080/
curl -i --data-binary @Cargo.toml http://127.0.0.1:8080/echo
```

### Tests

- 8 tests unitarios del parser: cortes en cada byte, pipelining, reglas de keep-alive, rechazos y serialización.
- 11 tests de integración contra el servidor real por TCP:
  - 500 requests en una sola conexión;
  - 50 requests en pipeline más un 404 final con cierre;
  - un request enviado byte a byte;
  - un cuerpo de 200 KB;
  - 64 clientes concurrentes con 50 requests cada uno;
  - límites, tope de conexiones y reutilización de slots;
  - apagado con conexiones ociosas abiertas.
- Miri sobre el parser. El anillo en sí no se puede correr en Miri porque usa syscalls reales.
- AddressSanitizer (nightly, `-Zbuild-std`) sobre los tests unitarios y de integración.

```sh
cargo test
cargo +nightly miri test --lib
RUSTFLAGS=-Zsanitizer=address cargo +nightly test -Zbuild-std --target x86_64-unknown-linux-gnu --lib --tests
```

### Mediciones (orientativas)

Las corrí en una VM Linux 6.12 de 8 vCPU (Intel Xeon), compartida y con otra carga. El generador de carga corre **en la misma máquina** que el servidor y compite por CPU. Son números de laboratorio, no una comparación contra otros servidores.

Carga en lazo cerrado con `cargo run --release --example loadgen -- <conexiones> 5`: `GET /health` en keep-alive, 5 s.

| Conexiones | req/s | p50 | p99 |
|---|---|---|---|
| 1 | 51 185 | 19.0 µs | 32.5 µs |
| 16 | 167 736 | 88.7 µs | 129.3 µs |
| 64 | 164 904 | 359.9 µs | 547.9 µs |

El servidor usa un solo hilo, así que a partir de ~16 conexiones el throughput se aplana y la latencia crece con la cola.

Parser (`cargo bench --bench parse`, criterion, mediana):

| Caso | Tiempo |
|---|---|
| `GET` con 3 cabeceras (73 B) | ~410 ns |
| `POST` con cuerpo de 1 KiB | ~386 ns |

### Limitaciones

- No soporta chunked, TLS, HTTP/2 ni `Expect: 100-continue`.
- No hay timeouts de inactividad: una conexión ociosa ocupa un slot hasta que el cliente la cierra o el servidor se apaga.
- Usa un solo hilo y un solo anillo. No usa buffers registrados, accept multishot ni `SQPOLL`.
- El cuerpo se copia a un `Vec` por request; el parser no es zero-copy.
- Requiere Linux ≥ 5.6 con `io_uring` habilitado (`kernel.io_uring_disabled = 0`).

Licencia: Apache-2.0.

---

## English

### What it is

One thread and one `io_uring` ring. Socket operations (`accept`, `recv`, `send` and `close`) are submitted to the ring and handled from the completion queue. The HTTP parser is hand-written too, in `src/http.rs`, with no `httparse`.

- HTTP/1.1 and HTTP/1.0: `GET`, `HEAD` and `POST` with `Content-Length`.
- Keep-alive (default in 1.1; in 1.0 only with `Connection: keep-alive`) and pipelining, with responses kept in order.
- Configurable limits: head size (431), body size (413) and header count.
- Explicit rejections: `Transfer-Encoding` → 501 (chunked is **not** implemented), unknown version → 505, bad syntax, obsolete line folding or a duplicate/invalid `Content-Length` → 400. The connection is closed in all of these cases.
- Connection cap: connections over the cap are closed right after accept.
- Clean shutdown through an `eventfd` read armed on the ring. All in-flight operations are drained before buffers are freed.

### Memory design

Each connection has **at most one operation in flight** (recv **or** send), so a buffer the kernel is using is never moved or modified until that operation completes. The read buffer is a fixed-address `Box<[u8; 16384]>`. `user_data` encodes `(op << 32) | slot_index`.

### Tests

- 8 parser unit tests: every split point, pipelining, keep-alive rules, rejections and serialization.
- 11 integration tests against the real server over TCP:
  - 500 requests on one connection;
  - 50 pipelined requests plus a final 404 with close;
  - a request sent byte by byte;
  - a 200 KB body;
  - 64 concurrent clients with 50 requests each;
  - limits, connection cap and slot reuse;
  - shutdown with idle connections open.
- Miri on the parser. The ring itself can't run under Miri because it uses real syscalls.
- AddressSanitizer (nightly, `-Zbuild-std`) on the unit and integration tests.

### Measurements (indicative)

I ran these on a shared 8-vCPU Linux 6.12 VM (Intel Xeon) with other load on it. The load generator runs **on the same machine** as the server and competes with it for CPU. These are lab numbers, not a comparison against other servers.

| Connections | req/s | p50 | p99 |
|---|---|---|---|
| 1 | 51,185 | 19.0 µs | 32.5 µs |
| 16 | 167,736 | 88.7 µs | 129.3 µs |
| 64 | 164,904 | 359.9 µs | 547.9 µs |

The server is single-threaded, so throughput flattens from about 16 connections on and latency grows with the queue. Parser medians: ~410 ns for a 73-byte `GET` with 3 headers, ~386 ns for a `POST` with a 1 KiB body.

### Limitations

- No chunked encoding, TLS, HTTP/2 or `Expect: 100-continue`.
- No idle timeouts: an idle connection keeps its slot until the client closes it or the server shuts down.
- Single thread and single ring. No registered buffers, multishot accept or `SQPOLL`.
- The body is copied into a `Vec` per request; the parser is not zero-copy.
- Requires Linux ≥ 5.6 with `io_uring` enabled (`kernel.io_uring_disabled = 0`).

License: Apache-2.0.
