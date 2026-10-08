# Opt-In HTTPS Gateway

This is an additive, local ingress bootstrap, not a production-verified topology.
The root Dockerfile, entrypoint, application, and CI smoke job remain unchanged.
The documented legacy-only deployment still works. Merging this code activates
nothing. There are no business handlers and `ENABLED_ROUTE_FAMILIES` is empty.

## Build And Check

Prerequisites: Docker, Rust (edition 2024), rustfmt, Clippy, Python 3, curl,
and OpenSSL. No Compose, Buildx, Harbour host installation, or new supervisor.

```sh
cargo build --locked --manifest-path gateway/Cargo.toml
cargo test --locked --manifest-path gateway/Cargo.toml
cargo fmt --manifest-path gateway/Cargo.toml -- --check
cargo clippy --locked --manifest-path gateway/Cargo.toml --all-targets -- -D warnings
docker build -t eshop-gateway:local gateway/
bash verification/local.sh
```

The runner builds both real images, creates disposable CA/leaf certificates,
networks and exclusive runtime volumes, compares direct and proxied application
scenarios on **separate** identically seeded Harbour instances, tests the native
`cargo run` entrypoint against a synthetic TLS server, checks failures and
isolation, and rehearses direct-publication rollback. It uses loopback ports
8002 and 18002; these must be free. Docker must support local published-port
access. Resources are removed on exit, including failed checks. Docker build
cache is retained. Native checks use temporary unprivileged listeners. TLS
volumes are seeded via `docker cp`, so Docker need not share the CLI filesystem.
No shared infrastructure or deployment is changed. CI may invoke this runner in
a separate future job; existing CI is not replaced.

## Configuration

The same environment applies to the image and native command:

```sh
cargo run --locked --manifest-path gateway/Cargo.toml --bin eshop-gateway
# Local check of the running operational listener:
gateway/target/debug/eshop-gateway healthcheck
```

| Variable | Contract |
| --- | --- |
| `PUBLIC_BIND` | Socket address, default `0.0.0.0:8002`; always TLS |
| `ADMIN_BIND` | Default `127.0.0.1:8003`, plain HTTP; never publish |
| `PUBLIC_CERT_FILE`, `PUBLIC_KEY_FILE` | Required readable PEM chain/key; validated before binding |
| `LEGACY_ORIGIN` | Required HTTPS origin, no credentials/query/fragment/non-root path |
| `LEGACY_CA_FILE` | Required PEM roots; chain and hostname verified; no bypass |
| `ENABLED_ROUTE_FAMILIES` | Comma-separated stable IDs; empty by default; unknown IDs fail startup |
| `CONNECT_TIMEOUT_SECS` | Default 5; TCP plus TLS deadline, also public TLS handshake |
| `UPLOAD_TIMEOUT_SECS` | Default 30; header-read timeout and body deadline after headers |
| `RESPONSE_TIMEOUT_SECS` | Default 60; upstream response headers/body deadline and handler deadline |
| `DRAIN_TIMEOUT_SECS` | Default 10; SIGTERM/SIGINT drain then cancellation |
| `LOG_LEVEL` | Default `info`; validated tracing level, not filter expressions |

Timeouts are integer seconds in 1..300; connect <= upload <= response. Upload
and response deadlines are total deadlines, not inactivity timers. Distinct
listeners are required. `/live` measures process availability; `/ready` performs
a trusted upstream GET `/hello` requiring exactly `200 Hello!`. The operational
listener must remain loopback/native or unpublished/container. Public `/live`,
`/ready`, `/hello`, `/info`, and unknown routes belong to Harbour.

## Ownership And Compatibility

`gateway/src/gateway.rs` is the only registration point: declare a handler module
and add its stable ID, complete literal path family (root plus slash descendants),
methods and function. A handler receives the streaming request and context with
generated request ID and direct peer address and returns an asynchronous streaming
response. Keep domain behavior in that handler, not transport or `legacy.rs`.
Use Harbour domain names (account, shopping, cart), not parallel invented models.
No transport, legacy adapter, or Harbour edits are needed for an independent route.
Registrations must have unique IDs, non-overlapping families, and nonempty unique
methods. Disabled/unknown/unowned requests proxy to Harbour. Enabling is separate
operator configuration; handler errors return 500/deadline 504, **never fallback**.
Do not migrate DBF/session behavior through this mechanism without another plan.

The adapter uses one verified HTTP/1 TLS connection per request and preserves
Host, raw encoded target/query, status, Location (including absolute and relative),
content type, body bytes, and separate cookies. It does not follow redirects,
store cookies, discover environment proxies, transform compression, or retry.
Known Content-Length bodies stream with backpressure and retain their framing.
Connection-nominated and standard hop-by-hop headers are removed both ways.
Complete Content-Length responses survive Harbour's missing TLS close-notify;
truncated bodies fail the downstream stream. Nothing disables TLS verification.
Connection failures return 502, response/connect deadlines 504. After response
headers are sent, stream failures close the connection, not a replacement status.
Slow upload errors can surface as 502 or connection closure, not a promised 504.

Explicit compatibility exceptions requiring operator approval before activation:

- Chunked ingress returns 411 and any Expect header returns 417. They are not
  forwarded to Harbour's Content-Length-only body reader. Hyper may send an
  interim 100 while parsing an Expect request; the terminal result is 417.
- A Connection nomination of Content-Length is rejected with 400 rather than
  stripping framing and risking a chunked request to Harbour.
- No client-supplied forwarding metadata is trusted: Forwarded, X-Forwarded-*,
  X-Real-IP, X-Request-ID, True-Client-IP and CF-Connecting-IP are stripped.
  Trustworthy metadata stays in context; no forwarded headers are synthesized.
- `/info` necessarily reports the gateway's upstream peer and TLS connection,
  not the public client's original transport. Do not blanket-normalize it.
- Header casing/order and HTTP framing for unknown-length **responses** may
  change. HTTP upgrades, tunnels and universal arbitrary streaming are not claimed.
- Gateway structured request logs contain only method, owner, status,
  header latency and generated ID. They do not assert successful body delivery.
  Legacy stdout still exposes forms and session cookies; restrict its access.

Characterization checks preserve current cart behavior: two adds yield total
53.34 and deletion can leave lines visible. They do not correct it. Sessions are
Harbour process-memory SESSID cookies with path `/`, refreshed 600-second expiry;
restart loses them and sharing is unsupported. Users/carts/items DBFs, indexes,
templates and static assets remain exclusively legacy-owned.

## Activation And Rollback

Activation is an explicit operator action only after compatibility gates pass:
provide a public certificate/key, identified endpoint owner, trusted upstream
certificate with the actual DNS identity, current exclusive runtime state,
approval of the exceptions above, and a rehearsed rollback. The generated legacy
CN-only localhost certificate is insufficient for private DNS identity.
Certificate automation, preceding proxies, production topology and deployment
responsibilities remain external prerequisites, not established repository facts.

Use two containers: Harbour on an internal network, **no published port**, with
the current `/app` state; gateway on private plus ingress networks, only HTTPS
8002 published, no `/app` mount. Keep public key material separate from legacy
key material. Mount TLS material read-only; give gateway only public TLS and the
legacy CA, never the legacy key. The runner uses read-only named TLS volumes and
one-time certificate symlinks in disposable state, without editing legacy code.
An existing deployment must quiesce and preserve its actual runtime directory;
never reseed it from checked-in tables. EXPOSE alone provides no isolation.

Executable rollback pattern (substitute **current** volume, selected TLS mount,
image and container names; stop ingress traffic first):

```sh
docker stop --time 15 eshop-gateway
docker rm eshop-gateway
docker stop eshop-legacy
docker rm eshop-legacy
docker run -d --name eshop-legacy -p 8002:8002 \
  -v CURRENT_STATE:/app -v LEGACY_TLS:/tls:ro harbour-eshop
curl --cacert LEGACY_CA https://localhost:8002/hello
# Log in again and verify account, shopping, cart; sessions were process-local.
```

The TLS mount pattern assumes existing runtime certificate symlinks like the
runner; otherwise restore the deployment's actual read-only certificate mounts
at `/app/private.key` and `/app/certificate.crt`. Never run two Harbour processes
against the same DBF/CDX directory. The runner proves same-volume account/cart
retention and explicit login after restart. Removing a future read-only route ID
returns ownership to legacy. Mutable-data rollback is not routing-only.

## Deferred Work

No business migration, session adapter, database selection/import/export,
domain crates, templates, workers, queues, caches, plugins, telemetry collector,
certificate automation, trusted-proxy CIDRs, Compose/Kubernetes, shared deployment
or production rollout. Capacity/load limits and malicious-client hardening need
deployment-specific assessment; this is a bounded local bootstrap, not a complete
edge-security product. Keep legacy logging/password risks visible rather than
claiming the gateway fixes them.

## Verification Results

Implementation verification passed using the available local tools:

- Locked native build, four Rust boundary tests, rustfmt check and warnings-as-errors
  Clippy; real unchanged Harbour Docker build and separate gateway Docker build.
- Real native `cargo run` and container entrypoints; 36 direct/proxy application
  hops with independent state/cookie jars, static bytes, validation retained values,
  account retries, navigation/pagination, login/logout and cart characterization.
- Synthetic verified TLS upstream: large/slow fixed-length uploads, encoded target,
  repeated cookies/Set-Cookie, Host, hop headers, unchanged absolute Location,
  response deadline 504, partial/slow body failure, cancellation without replay,
  wrong hostname and unrelated CA failure, and in-flight bounded SIGTERM drain.
- Invalid TLS/configuration/activation inputs, incomplete headers/uploads, upstream
  unavailable/restored, process/readiness checks, listener-level activation and
  handler failure without fallback, private-port/mount isolation, same-state rollback.
- Existing Rust `-C instrument-coverage` instrumentation confirms exercised counters
  increase from 0 (all tests skipped) to 6,518 of 18,339 (four tests). This includes
  dependency code; it is **not** a source-line coverage percentage. LLVM report
  tooling is unavailable, so no line/branch percentage is claimed or tool installed.
- Disposable runner containers, networks, volumes, certificates and image tags were
  removed after success and earlier failed runs. Root files and legacy CI are unchanged.

Remaining gates are external deployment topology/certificate/state ownership and
operator acceptance of compatibility exceptions. This does not verify production
load, session expiry after 600 seconds, arbitrary HTTP upgrades/chunked bodies,
every malformed request, trusted preceding proxies, or mutable-domain rollback.
There is no migrated application work to activate; those slices remain deferred.
