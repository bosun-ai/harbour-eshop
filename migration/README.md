# Opt-In HTTPS Boundary

The gateway is a separate process and image. Installation does **not** activate it.
The root Dockerfile, entrypoint and all Harbour handlers/runtime inputs are unchanged.
The initial registry and activation list are empty: every application request goes
to Harbour. No application domain adapters, rendering, data or session sharing exist.

## Build And Verify

Requirements: recorded Rust toolchain, Docker, Python 3, openssl and network access
for locked crate downloads/base images/Harbour source. No Compose or deployment
platform is assumed. Run on an isolated local machine with ports 8002, 9000 and
18002 available; verification creates disposable runtime copies and deletes them.

```sh
cargo build --locked --manifest-path gateway/Cargo.toml --release
cargo test --locked --manifest-path gateway/Cargo.toml
cargo fmt --manifest-path gateway/Cargo.toml --check
cargo clippy --locked --manifest-path gateway/Cargo.toml --all-targets -- -D warnings
docker build -t harbour-eshop:gateway-test .
docker build -t harbour-eshop-gateway:gateway-test -f gateway/Dockerfile gateway
python3 migration/verify.py --binary gateway/target/release/eshop-gateway \
  --legacy-image harbour-eshop:gateway-test \
  --gateway-image harbour-eshop-gateway:gateway-test
```

For native execution, copy `gateway.example.toml`, change certificate paths,
listener addresses and upstream origin, then run:

```sh
./gateway/target/release/eshop-gateway --config /absolute/path/gateway.toml
```

One TOML file is mandatory; there is no environment discovery or insecure TLS
switch. Unknown fields, invalid timeouts/listeners/origins/activation IDs and
unreadable or mismatched public TLS material fail before binding. Upstream trust
and SAN are authenticated at connection time; failures make `/ready` return 503.
Public TLS is not automatically provisioned. Supply separate public cert/key and
upstream CA PEM. Certificates/keys must be readable by container UID 65534. Mount
public inputs read-only and give the gateway **only** upstream trust, not the
Harbour private key or `/app`. Example paths refer to the container namespace.

Management HTTP binds loopback/private addresses only; never publish its port.
`GET /live` reports process health, `GET /ready` verifies TLS and a bounded
`/hello` containing `Hello!`. It is not a DBF integrity check. Public `/hello`,
`/live` and `/ready` are all legacy-owned paths.

## Protocol And Lifecycle

The adapter uses HTTP/1.1 only, a new upstream connection per request, no pooling,
retries (including mutating GETs), redirects, cache, decompression or replay. It
preserves raw origin-form path/query, method, public Host, body bytes, cookies,
status, Location and repeated end-to-end headers. Connection-nominated and other
hop headers are removed. Connection establishment/TLS identity use the configured
upstream origin, independently of Host. Chunked requests get 411, ambiguous/invalid
framing gets 400 (Hyper may reject before dispatch). No whole-body buffering or
chunked fallback exists. Absent Content-Length means zero bytes, per HTTP framing.
CONNECT, absolute-form targets and upgrades get 400; other methods are forwarded
and hbhttpd normally returns 501. HTTP/2 is not advertised. Informational responses
and trailers are not a compatibility promise; request chunking/upgrade support
requires an explicit later decision. Connections close after each public request.

Untrusted peers' Forwarded/X-Forwarded-* and request IDs are discarded. The gateway
generates authoritative X-Forwarded-For/Host/Proto; configured trusted proxy CIDRs
may contribute only a syntactically valid IP chain. TLS is still required from
such peers. No application authentication decision uses these fields here.

502 means connection/TLS/exchange failure; 504 means connect or total response
deadline. Failure after response headers terminates the stream, not a new status.
A failure after submission does **not** prove a mutation failed: do not replay it.
Responses stream with backpressure; cancellation aborts the upstream task. Body
idle limits apply in both directions, total response deadline includes submission
and streaming. Public TLS/connect, header, body idle, response and drain settings
are separate positive values bounded to 300 seconds. Example limits are 5/30/30/
120/30 seconds, not an upstream performance SLA. Pinned hbhttpd uses 30-second
TLS/header and 120-second body receive limits, with 1-second IO polling. Ingress
allows 128 public and 8 management connections, drops excess accepted connections,
and caps request-header buffering at 32 KiB (management 8 KiB). SIGTERM/SIGINT stop
accepting, drain current work for the configured deadline, then cancel leftovers.
No public keepalive minimizes idle lifecycle/resource ambiguity.

Structured gateway logs contain generated ID, known method (or OTHER), classification,
status and time to response headers, not raw URL, credentials, headers or body.
Post-header stream failure is observed by the client, not recorded as a new status.
Harbour **still logs passwords and session cookies**; protect its logs accordingly.

## Ownership And Future Slices

`FamilyDescriptor` requires a stable lowercase family ID, nonempty complete raw
path selectors and explicit methods. Prefix selectors end in `/`; encoded selectors,
query selectors, whitespace and overlapping paths (even inactive families) fail
startup. Methods not declared remain legacy-owned. Declare every supported method
for a slice, including any explicit rejection behavior. `SliceHandler` receives and
returns streaming bodies; dependencies belong to the slice, not the adapter.
Add implementation/registration only in `gateway/src/slices/mod.rs` and activate
separately in `active_families`. Inactive registrations always proxy. Once selected,
handler failure never falls back; that could duplicate a mutation. No Harbour edit
or new dispatcher architecture is needed. Authentication and mutable-state ownership
must be solved before a real slice can be enabled.

## Activation And Rollback

No shared deployment is performed or evidenced. Before local activation:

1. Retain the current image identity and full current runtime. Quiesce any existing
   writer before copying runtime; verify transfer checksums. Do not copy seed DBFs
   over live files. A full `/app` mount hides the image executable/templates too:
   retain `eshop`, `tpl/`, `files/`, all DBFs/CDXs and TLS files. Provision Harbour
   `private.key`/`certificate.crt` with SAN `legacy` (and localhost for local tests).
   Its existing entrypoint accepts these files. Exactly one process may write DBFs.
2. Build and verify disposable fixtures, then test an alternate ingress endpoint
   before the explicit public-port handoff. The example activation script binds
   8002; adapt host mapping for a rehearsal, never run the mutation corpus on live
   state. Save the runtime path, image identity, trust and fallback commands.
3. Supply a gateway-only TLS directory (`public.key`, `public.crt`, `legacy-ca.crt`)
   and a config using the example container paths. Call the foreground script:

```sh
./migration/run-gateway.sh --activate-ingress LEGACY_IMAGE GATEWAY_IMAGE \
  /absolute/current-runtime /absolute/gateway.toml /absolute/gateway-tls
```

The script creates a dedicated internal network, runs Harbour without published
ports, and publishes only gateway 8002. It refuses a runtime mounted by another
Docker container; the operator must also exclude native/external writers. It never
initializes runtime. It cleans up on exit/interrupt, using Harbour's `//stop` before
stopping the container and retaining runtime. Its cleanup is local, not a port
switch/zero-downtime deployment system. A gateway failure does not overwrite state.

For a Docker daemon with a separate filesystem, the script also accepts
`--activate-ingress-volumes LEGACY_IMAGE GATEWAY_IMAGE RUNTIME_VOLUME INPUT_VOLUME`.
Both volumes must already exist. The inputs volume contains `gateway.toml` and
only gateway TLS/trust files; use absolute paths under `/inputs` in that config.
The script validates the runtime with a read-only, networkless helper container.
The verification harness uses this real entrypoint and disposable named volumes
populated via `docker cp`, proving the same exclusive-writer/persistent-runtime
behavior without depending on local bind-path visibility. Only the gateway joins
an additional non-internal ingress network, needed for rootless published ports;
Harbour stays on the dedicated internal network only.

Rollback prefers retaining the running Harbour process when an existing ingress
mechanism permits; none is supplied here. With Docker port recreation:

1. Quiesce clients; stop/drain the gateway. Invoke `docker exec LEGACY ./eshop //stop`,
   wait for the sole writer to exit (`docker wait LEGACY`), then checksum current
   DBFs and retain the entire runtime. Remove the old stopped container before
   starting a replacement; never run two writers.
2. Restore direct access with the recorded image and **same** runtime:

```sh
docker run --rm -p 8002:8002 --mount type=bind,src=/absolute/current-runtime,dst=/app LEGACY_IMAGE
```

3. Verify `/hello`, re-login, stored account name and cart totals. Harbour restart
   loses existing sessions, not stored accounts/carts. Preserve the public cert or
   explicitly handle certificate changes: the private `legacy` cert may not match
   the public name, so provision appropriate SANs before exposing it directly.
   Verification's disposable certificate includes both legacy and localhost.

Gateway replacement leaves the running writer/session/runtime intact. Rollback
is state-preserving only because bootstrap owns no application data; this contract
does not extend to future mutable-data slices.

## Recorded Legacy Exceptions And Deferred Work

Pinned hbhttpd treats SESSID as opaque, process-local, 600-second sliding timeout;
no cross-runtime session sharing contract exists. Its cookie parser handles only
the first name/value per Cookie header and can retain a semicolon in multi-cookie
forms; the adapter does not repair it. Cookie attributes are unchanged (including
legacy's missing Secure/HttpOnly flags). Tests use independent cookie jars and
normalize **only** generated SESSID values for corpus comparisons.

`/info` reflects addresses, TLS details and added forwarding headers; that body's
topology differences are the sole diagnostic comparison exception, not permission
to ignore arbitrary body differences. Users and carts start empty; items contain
29 books. Cart AMOUNT/TOTAL schema is `app/eshop.prg:62`, item PRICE is
`app/eshop.prg:73`. No password hardening, session migration, domain/data workers,
application rendering or route migration is part of this bootstrap. Durable storage,
production DNS/TLS, secret delivery and deployment ownership remain external
operator prerequisites, not infrastructure created by merging these files.
