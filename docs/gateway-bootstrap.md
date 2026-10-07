# Optional Gateway Bootstrap

## Changes

This is an implemented, opt-in HTTPS ingress, not an existing production gateway or deployment. Harbour remains the sole owner of routes (including unknown routes and unsupported methods), sessions, templates, files, DBF/CDX state and mutations. No files in `app/`, the legacy Dockerfile or entrypoint change. Existing `docker build -t harbour-eshop .` and `docker run --rm -p 8002:8002 harbour-eshop` remain the default.

The standalone `gateway/` crate uses locked Tokio, Hyper HTTP/1 and Rustls dependencies. `dispatch.rs` defines ownership independently of integration; `legacy.rs` is the opaque TLS adapter. `slices::register()` is the permanent, initially empty extension point. A later family implements `HttpHandler`, declares its complete raw-path and method ownership, and registers there. Only `enabled_families` in the explicit config activates it. Registration alone changes nothing. Unknown IDs, empty/duplicate method declarations and overlapping ownership fail startup. Unmatched traffic stays legacy; active-handler errors never fall back or replay.

## Build And Local Run

```sh
cargo build --locked --release --manifest-path gateway/Cargo.toml
cargo test --locked --manifest-path gateway/Cargo.toml
cargo fmt --manifest-path gateway/Cargo.toml -- --check
cargo clippy --locked --manifest-path gateway/Cargo.toml --all-targets -- -D warnings
docker build -f gateway/Dockerfile -t harbour-eshop-gateway:bootstrap .
gateway/target/release/eshop-gateway --config /absolute/path/all-legacy.toml
```

The recorded toolchain is Rust 1.97.1; the gateway image uses that toolchain. The executable accepts only `--config FILE`. Configuration parsing/validation and certificate loading happen before listeners start. Errors report categories, not config values or TLS details. The example file is not deployment-ready: replace mount paths and deliver valid credentials. Port `8002` is fixed for Harbour; the URI must be HTTPS with root path, no credentials, query or fragment. Its hostname is the verified upstream SAN. Public TLS and upstream trust are independent; neither has an insecure mode. Private management `/livez` and `/readyz` are HTTP, never public application routes. Readiness performs a verified HTTPS `/hello` probe and checks exactly `Hello!`; it does not prove DB correctness.

## External Prerequisites

- Deliver a matching public certificate/key for the established public hostname. Mount config and only `public.crt`, `public.key`, `upstream-ca.crt` read-only in the gateway credential directory. UID 65532 must be able to read/traverse them; use appropriate owner/group permissions, not world-readable production keys. Never put Harbour's private key in that directory.
- Deliver matching `private.key` and `certificate.crt` in Harbour's exclusive runtime before startup, with SAN `legacy` for the launcher network alias. The unchanged entrypoint's CN-only self-signed fallback is not valid upstream provisioning. The gateway needs explicit trust material, not that private key.
- Initialize retained runtime from the **recorded legacy runtime image**, including executable and assets, not a source checkout: `docker create --name runtime-seed IMAGE`, `docker cp runtime-seed:/app/. /absolute/runtime`, then `docker rm runtime-seed`. Preserve existing DBF state on cutover; record ownership, backups and image IDs. Harbour needs writable indexes, logs and stop marker. Users schema is at `app/eshop.prg:47`, carts at `app/eshop.prg:62`, items at `app/eshop.prg:73`.
- Docker must see all absolute bind paths on its host. With a remote daemon, stage files on that host. Restrict daemon/network administrative access: an administrator can attach unauthorized containers. The launcher creates a two-peer internal upstream network and a separate gateway-only ingress network. Harbour has no published ports; management remains loopback. No application bind-address change is invented.
- Confirm observed workload fits configured limits, issue certificates through the established delivery process, and approve the transport differences below before listener cutover. Building/merging is not activation.

## Explicit Launcher

```sh
scripts/run-gateway.sh --activate \
  sha256:RECORDED_LEGACY_IMAGE_ID sha256:RECORDED_GATEWAY_IMAGE_ID \
  /absolute/all-legacy.toml /absolute/gateway-credentials /absolute/runtime
```

Only immutable local IDs or digest references are accepted. Use the example's container binds, paths and alias with `enabled_families = []`. The foreground launcher validates required files, reserves a runtime writer lock, starts private Harbour and then the gateway, and polls private readiness at most 30 times. Do not send traffic until readiness succeeds. The public listener can bind before readiness; requests then fail with 502/504 rather than silently replay. Startup failure or SIGINT/SIGTERM stops/removes its containers/networks and releases the lock without deleting runtime data. It allows 12 seconds for Docker stop; keep configured gateway drain below this bound. Never bypass the writer lock, launch a second direct writer, or remove it until a stranded writer is confirmed stopped. Gateway shutdown alone never writes `.uhttpd.stop` or stops Harbour; stopping the launcher owns both container lifecycles.

## Transport And Resource Policy

Original valid Host, raw path/query, body bytes, opaque cookies, status, Location (even absolute), content type, distinct Set-Cookie and end-to-end fields survive. Upstream transport is HTTP/1.1 with `Connection: close`; hop fields and Connection-nominated fields are removed. There is no retry, pooling, redirect following, compression, cache, mirroring or response rewriting, including for mutating GETs. One upstream connection per request avoids replay/pool ambiguity and trades connection setup cost for a simple lifecycle.

Accepted requests have no Transfer-Encoding, Expect or upgrade, origin-form targets and exactly one valid Host. Optional single numeric Content-Length is streamed with exact byte-count checks; absent length means zero. Chunked, ambiguous framing, absolute-form targets and Connection-nominated Host/length are rejected with 400 and closed; oversized declared bodies return 413. Hyper may reject malformed wire headers itself before dispatch. These are **intentional compatibility differences**, not claims of universal HTTP transparency. Default limits: 100 headers, 32 KiB parser buffer, 1 MiB declared request body, 64 public connections (including TLS/idle keep-alive), eight management connections. Saturation closes new sockets rather than allocating an unbounded queue. Response bodies stream without accumulation; readiness collects at most six bytes. Timeouts are positive and capped at five minutes. Defaults: connect/probe 3s, header/body idle 10s, total request 30s, shutdown drain 10s. Header limit is Hyper's parser buffer bound, not a byte-perfect sum of field lengths.

TLS/connection failure before response headers maps to 502, connect/header/total deadline to 504. A request-stream failure may surface as upstream transport failure (502). After response headers, failed/idle/deadline streams terminate without appending an error page. A timeout, cancellation or closed connection does **not** establish whether a Harbour write committed; never retry automatically. Dropping response bodies aborts their upstream driver. Shutdown drops listeners, gracefully drains connections, then aborts remaining tasks at the configured bound.

Trusted proxies are empty by default. Trust is an explicit list of exact peer IPs, not CIDRs. Only a single parseable `X-Forwarded-For` IP from such a peer can replace client IP; ambiguous chains fall back to actual peer. All client Forwarded/X-Forwarded-* and X-Request-ID are stripped and canonical client IP, HTTPS scheme and a generated correlation ID inserted. Logs emit JSON: correlation ID, method, owner, status, duration to headers and transport failure category. They do not log paths, queries, headers, bodies, credentials or cookies. Legacy access logs retain legacy behavior and require their existing access controls.

`/info` is still a real proxied page. The comparison harness permits only enumerated differences in remote/local socket fields, TLS cipher/protocol/key-size fields, Connection/length and generated forwarding/ID fields. It normalizes only observed independent SESSID values and omits diagnostic Content-Length because normalized transport rows change length. Host, raw URI/query, method, platform, certificate identity and other rendered bytes remain compared; there is no blanket `/info` exclusion.

## Checks And Results

```sh
python3 verification/compatibility.py --legacy-image harbour-eshop
python3 verification/transport.py
python3 verification/deployment.py --legacy-image harbour-eshop --gateway-image harbour-eshop-gateway:bootstrap
```

Executed locally: unchanged legacy Docker build and exact `/hello` with certificate validation; gateway locked release build, Docker build, contract tests, formatting and strict Clippy; 31 direct-versus-proxy synthetic stateful scenarios and every redirect hop on independent zero-user/zero-cart/29-item seeds. Covered registration errors/duplicate/success, failed and successful login, logout, protected routes, retained forms, name/password edits, shopping pages, repeated adds, cart deletion/totals, CSS/missing files, unknown paths/methods and narrowly normalized `/info`. Restart proves account/cart DBF persistence and in-memory session invalidation.

The controlled TLS upstream verifies Host/raw query/bytes, declared framing, separate cookies, hop stripping, no retries including dropped mutating GET, header deadlines, partial/slow response termination, slow request bodies, cancellation recovery, untrusted public/upstream certificates, wrong upstream SAN, invalid activation and bounded SIGTERM drain with a live request. Deployment checks exercise the actual launcher/image entrypoints, immutable IDs, private networks/port publication, exclusive runtime lock, read-only gateway mounts, cleanup and restart-based direct rollback using current DBFs. Synthetic credentials/diagnostic responses stay temporary and are not CI artifacts. The remote-daemon test stages only disposable fixtures in daemon `/tmp` to exercise the same real bind mounts.

Coverage is measured with Rust `-C instrument-coverage`, not estimated from test counts. `verification/coverage.py` reads validated LLVM raw-v10 64-bit execution counters for gateway symbols, compares an invalid-startup baseline with tested runs and requires increased execution. It is **not source-line or branch coverage**. Observed baseline: 24/5365 counters; five contract tests plus transport/compatibility: 2060/5373 (38.3%, including instantiated generic functions). CI repeats the measurement without adding a coverage tool. Run in clean temporary directories; raw profiles can differ as code changes. CI compiles/tests in the pinned Rust image and uses runner-provided formatting/Clippy tools; the minimal Rust image itself lacks those components.

CI retains the legacy smoke, adds exact-body assertion and separately runs all optional checks with temporary synthetic TLS. It has no deployment step. Passing CI does not authorize public activation or establish production capacity. Limits and deadlines still require operator acceptance against the intended workload, not deferred implementation verification.

## Activation And Rollback

Record immutable images, current exclusive runtime and credential/trust ownership. Pass these checks with representative config; approve framing/diagnostic differences. Stop/quiesce the existing writer before cutover. Current Docker publication usually requires container recreation and a maintenance window: Harbour sessions are process-memory with sliding 600s expiry and will be lost. Preserve current DBFs, not image seed copies. Start all-legacy topology explicitly, check public identity/readiness/Hello! and an authenticated synthetic flow.

Before activation, removing the optional gateway leaves the direct deployment unaffected. After activation, withdraw its listener, stop/quiesce its Harbour writer, then start the **recorded legacy image** with `8002:8002` attached to the **current runtime** and existing credentials. Never restore an old snapshot simply because ingress changed, and never attach two writers. Recheck login and persisted account/cart data; expect session invalidation. A separately established external traffic switch could allow routing-only rollback to a still-running Harbour process, but none is provided here. Rollback covers ingress only, not future Rust-owned state.

## Deferred Application Work

No application family, domain behavior, session sharing, DBF adapter, schema migration, transaction/outbox, shared filesystem, worker or data ownership moves here. Later slices use the existing handler registration and the same explicit activation list without further legacy integration edits. Any mutable Rust-owned state needs its own compatibility and rollback plan.
