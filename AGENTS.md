# AGENTS.md

## Essential commands

```sh
cargo build
cargo test                    # all unit + integration tests
cargo test --doc              # doc tests
cargo clippy --all-targets --all-features -- -D warnings   # CI-enforced; warnings are errors
cargo fmt --all               # formatter
cargo machete                 # check for unused dependencies (runs in PR CI)
cargo llvm-cov --all-features --workspace --codecov --output-path codecov.json  # coverage
```

Order when verifying before a commit: `fmt → clippy → test`.

Pre-commit hooks run `fmt` and `clippy` automatically on staged files (`.pre-commit-config.yaml`).

## Project structure

Single-crate library — **no workspace, no binary**.

| File | Purpose |
|---|---|
| `src/lib.rs` | Server logic: `Lwm2mServer`, `MessageHandler` (Tower service), CoAP routing |
| `src/transport.rs` | `Transport` trait, `UdpTransport`, test-only `InMemoryTransport` |
| `tests/test_udp_server.rs` | Integration tests — binds a real UDP socket on `[::1]:5683` |

## Technical Requirements (Planned)

- LwM2M 1.1 spec
- CoAP transport on UDP
- no security (NoSec) on transport (physical network layer is handling it)
- CoAP blockwise transfer (block size: 64)
- separate ACK with timeout 30s
- CBOR and SenML-CBOR
- LinkFormat parsing
- LwM2M server and bootstrap server
  - bootstrap (r/w/d)
    - Bootstrap request 
    - Write request 
    - Delete request 
    - Bootstrap finished request 
    - Bootstrap Device Initiated 
  - registration
    - Register
    - Register update
    - De-register
  - maintenance (r/w/x)
  - send/notify
  - no Observe
- client registry
- retransmission/retries (exponential backoff)
- deduplication (caching)
- CoAP max message size: 1024
- LwM2M object (IPSO) registry
- eventually PyO3 bindings
- possibly diff-tests (and diff-fuzzing) with wakaama (using wakaama-rs wrapper, branch: wakaama-rust-bindings-threading)

## CI matrix

- Builds and tests on Rust `stable`, `nightly`, `1.81`.
- Sanitizer CI (`sanitizers.yaml`) runs on **nightly** with `-Zbuild-std --target x86_64-unknown-linux-gnu`. Sanitizers: address, leak, memory (with origin tracking), thread, safestack.

## Spec PDFs

`specs-pdfs/` contains LwM2M and CoAP specification PDFs. The `pdf-reader` MCP server is configured to serve them from that directory — use it to look up protocol details without leaving the session.

## Agent definitions

Additional agent instruction files at the project root:

- `AGENT-DOC.md` — documentation agent guidance
- `AGENT-test.md` — test agent guidance

