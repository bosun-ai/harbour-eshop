# Optional HTTPS Gateway

This standalone Rust executable is an **opt-in all-legacy bootstrap**, not an application migration or production deployment. Every public path, including `/hello`, `/info`, `/files/*`, `/live`, `/ready`, and unknown routes, falls back to one Harbour HTTPS process. The legacy source, image and entrypoint are unchanged. Merging this directory activates nothing.

## Build And Verify

Requirements: Rust/Cargo, Docker, Python 3 and OpenSSL. No Compose, Harbour installation or Python packages are needed. The harness pulls `python:3-slim` for its disposable HTTPS protocol upstream. Image base tags and package inputs remain mutable; Cargo dependencies are locked, and the root image pins Harbour by default (its build argument remains overridable).

```sh
cargo build --locked --manifest-path gateway/Cargo.toml
cargo test --locked --manifest-path gateway/Cargo.toml
cargo fmt --manifest-path gateway/Cargo.toml -- --check
cargo clippy --locked --manifest-path gateway/Cargo.toml --all-targets -- -D warnings
docker build -t harbour-eshop:baseline .
docker build -f gateway/Dockerfile -t eshop-gateway:bootstrap gateway
python3 gateway/tests/verify.py \
  --legacy-image harbour-eshop:baseline \
  --gateway-image eshop-gateway:bootstrap
```

The standard-library runner creates disposable containers, a network, read-only gateway input volumes, and separate SAN-valid certificates. It stages inputs with `docker cp`, so Docker need not share the CLI filesystem. The gateway volume contains **only** public identity/key, upstream trust certificate and config; never Harbour's private key or `/app`. The legacy container retains its own writable runtime. Direct legacy, gateway and management ports are published on **host loopback only for comparison**. Production must publish only gateway port 8002; management is private. Cleanup occurs on success and exceptions. Do not run this corpus against real accounts or production storage.

The native entrypoint is exercised when `gateway/target/debug/eshop-gateway` exists; `--gateway-binary` overrides it. A missing binary prints an explicit gap. The corpus checks redirects, unknown/unsupported routes, protected navigation, CSS/conditional responses, retained registration errors, registration/login/logout, account edits, pagination, cart add/delete and totals. Writes are performed once against one disposable account, and direct/proxy comparisons reuse its same opaque session. It checks gateway restart, Harbour restart, readiness failure and same-owner direct rollback. A separate HTTPS upstream checks exact raw URI/body/length, duplicate cookies, hop headers, 2 MiB streaming, stalls, disconnects, no retries, TLS trust/hostname failures, header/body timeouts, graceful and forced drain, sanitized logging and invalid startup/activation.

## Local Run

Supply SAN-valid Harbour certificate/key at `/app/certificate.crt` and `/app/private.key` **before** startup. The legacy entrypoint's default CN-only certificate is not a valid normal Rust TLS identity for `legacy`. Use one private legacy container with network alias `legacy`, no public port, and one exclusive retained writable runtime. Do not invent a Harbour bind environment variable; Harbour binds port 8002 on all interfaces inside its container.

Copy `config.example.toml`, adjust the listener addresses, upstream URL and readable PEM paths, then run:

```sh
GATEWAY_CONFIG=/absolute/path/config.toml gateway/target/debug/eshop-gateway
# Or, on the same private Docker network as legacy:
docker run --rm --network YOUR_PRIVATE_NETWORK -p 8002:8002 \
  -e GATEWAY_CONFIG=/run/gateway/config.toml \
  -v /absolute/path/gateway-inputs:/run/gateway:ro eshop-gateway:bootstrap
```

The image directly executes `/usr/local/bin/eshop-gateway` as UID/GID 65532. Input directories must be traversable and files readable by that UID; use deployment-appropriate ownership/modes for the public private key. The harness's world-readable disposable keys are **not** a production permissions recommendation. No shell supervisor, bundled application, certificate generation or secret environment variables exist in the gateway image.

## Configuration Contract

`GATEWAY_CONFIG` is required. TOML rejects unknown fields and invalid values; errors expose only fixed classifications, never parsed configuration or paths. All example values are local test policy, not established production limits.

| Field | Contract |
|---|---|
| `public_bind` | Explicit socket address and nonzero port, normally 8002 |
| `management_bind` | Separate port, unpublished in deployment; HTTP `GET /live` and `GET /ready` only |
| `upstream_url` | Fixed HTTPS authority, root base path, no credentials/query/fragment; normally `https://legacy:8002/` |
| `public_certificate`, `public_key` | PEM identity with matching key; validation precedes binding |
| `upstream_ca` | Explicit nonempty PEM trust bundle; hostname checking cannot be disabled |
| `trusted_proxies` | Exact IPs, empty by default; only a trusted peer's X-Forwarded-For chain is retained and appended |
| `enabled_families` | Whole-family allow-list, empty by default; bootstrap has no registered application families |
| `log_level` | `error`, `warn`, `info`, `debug`, `trace`; dependency events are filtered out at every level |
| `limits` | Required millisecond values, all 1..3600000; total must be at least upstream-response limit |

Limits separately bound TCP/DNS/TLS connect (including public TLS handshake), client headers, body-frame idle, upstream response headers, total request/stream, readiness and shutdown. The example total exceeds Harbour's observed 120-second body-read deadline. Readiness performs a bounded `GET /hello` with the same fixed-authority client and trust, requiring 200 and exactly `Hello!` (maximum 64 bytes). Liveness does not depend on Harbour. Syntactically valid but incorrect upstream identity/trust allows startup, then reports 502 and unhealthy readiness; missing/malformed TLS files and mismatched public key fail startup.

SIGINT/SIGTERM close both listeners, mark shutdown, disable keepalive acceptance and drain accepted connections. The shutdown deadline aborts outstanding streams. Handler context exposes a generated request ID, actual peer, total deadline and shutdown receiver without domain/session state. An accepted handler can complete during drain. Idle clients are bounded by header timeouts; streaming bodies by idle/total limits. There is no connection-count/rate admission policy yet: exposure to hostile ingress needs deployment capacity controls and load qualification.

## Compatibility And Ownership

- Preserve method, raw path/query, bytes, public Host, cookies, status, raw relative/absolute Location and repeated response headers. Fixed upstream authority controls DNS/TLS regardless of Host. HTTP/1.1 only; no redirects, cookie jar, decompression, HTML rewriting or retries, **including GET** cart mutations.
- Preserve valid known Content-Length. Reject chunked ingress with 411 rather than buffering or sending undecodable chunked forms to Harbour. Ambiguous/malformed framing and protected Connection-nominated headers are rejected with 400; Hyper may reject malformed framing before dispatch. HTTP/2, upgrades, CONNECT tunneling and request trailers are not supported. This is an explicit compatibility gate, not universal proxy equivalence.
- Strip hop-by-hop and Connection-nominated headers; response lengths remain valid. Untrusted forwarding metadata is removed; gateway adds actual peer X-Forwarded-For, `X-Forwarded-Proto: https` and generated `X-Request-ID`. Incoming correlation IDs cannot override it; response `X-Request-ID` is gateway-owned.
- Connect/TLS/early stream failures become sanitized 502; response/total deadline expiry becomes 504 before headers. A stalled request-body stream can become 502. Failures after headers abort the connection, never substitute a response. Timeouts and ingress rejection are new observable policy.
- `/info` deliberately differs: forwarded metadata, request ID, upstream socket peer and transport values are visible. Bodies elsewhere are compared without blanket exclusions; only session tokens are normalized. Header casing/order among different names is not a wire guarantee; repeated values per name retain their order.
- Gateway JSON events contain only allow-listed fields. Access status/duration describe **response headers**, not confirmed delivery; late stream failures generate a sanitized connection event. Legacy tracing still prints headers/bodies/cookies; the gateway cannot promise system-wide secret-free logs. Protect legacy diagnostic/log access separately.
- Harbour owns DBFs and process-local opaque SESSID cookies (600-second inactivity lifetime). Gateway restart preserves its sessions; Harbour restart loses sessions but retains accounts/carts only when its complete runtime is retained. There is no shared session service or second writer.

Future slices implement one private `GatewayHandler` and add one `FamilyRegistration` in `main.rs`; transport, legacy adapter and static serving need no edits. Registration is inert without explicit `enabled_families`. A path matcher owns an exact root and slash-delimited descendants, with all nine standard methods; unknown extension methods fall back to Harbour. Startup rejects duplicate IDs, overlapping matchers, incomplete method sets, duplicate selections and unknown activation IDs, even for inactive registrations. Registry tests prove inactive/active decisions; no demonstration application route ships.

## Activation And Rollback

Production switching is an external operator action, not defined by this repository. Required inputs: ingress ownership and endpoint switch mechanism, correct public TLS identity, SAN-valid upstream identity/trust, private networking, private probe access, exclusive durable complete legacy runtime, retained artifact/config versions, capacity policy, and deployment-specific compatibility checks. Run only all-proxy mode initially; require readiness and the corpus before directing public traffic. Avoid restarting Harbour during the switch, or explicitly accept forced re-login.

Rollback directs traffic back to **the same exclusive legacy state owner**, then stops gateway routing. Do not create a seed container against empty storage or let two Harbour processes open the same DBFs. If recreation is unavoidable, retain the complete current runtime first; restarting Harbour loses sessions. Recheck TLS, exact `/hello`, login, catalogue and cart totals. Disabling a future family is not an automatic mutable-data rollback.

## Deferred Work And Gaps

No application domain logic is migrated: no DB/session adapters, domain types, DBF exports/transfers, rendering, workers, queues, caches, APIs, plugins, shared libraries, metrics platform, certificate automation, orchestration or migration order. TLS/framing/session/diagnostic/storage questions remain deployment constraints rather than an assertion of a production boundary.

Verification is representative, not exhaustive: no production load/DoS qualification, every malformed HTTP permutation, all browsers, real ingress switching, 120-second legacy timeout soak, multi-instance sessions or mutable application rollback. No automatic persistence setup is provided. Legacy secret-bearing tracing remains unchanged. Cargo unit tests cover configuration, registration/activation, framing/header stripping, metadata and body deadline/size hints; black-box checks cover real entrypoints and failures.

Implementation validation: root Harbour image build and separate gateway image build passed; locked native build, six Rust unit tests, formatting and Clippy with warnings denied passed. The full disposable corpus passed, including native local run, trace-level log filtering and both shutdown outcomes; containers, networks and input volumes were removed. Python standard-library trace measured 385/444 runner executable lines (86.7%; the HTTPS upstream executes in separate containers). Rust coverage-instrumented tests also passed and emitted nonempty profiles; no LLVM coverage-report tools are available in the environment, so a Rust line/branch percentage is not claimed. There was no prior gateway test coverage to compare numerically.
