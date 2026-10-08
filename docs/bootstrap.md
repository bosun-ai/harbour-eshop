# Opt-In HTTP Boundary

This is a sample Docker HTTPS topology, **not a production deployment**. The
default root image, entrypoint, Harbour sources, and legacy CI smoke are unchanged.
Building, merging, and running the checks do not activate a deployment or a Rust
application route. Bootstrap has zero independent application registrations and
no application/domain implementation, session adapter, or data migration.

## Build And Check

Prerequisites: Docker CLI and a reachable daemon, Rust/Cargo with Rustfmt and
Clippy, Python 3, OpenSSL, and access to image registries, crates.io and the pinned
Harbour download. Compose and buildx are not required; no tools are installed.

```sh
cargo build --locked --manifest-path gateway/Cargo.toml
cargo fmt --manifest-path gateway/Cargo.toml -- --check
cargo clippy --locked --manifest-path gateway/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path gateway/Cargo.toml
docker build -t harbour-eshop:boundary .
docker build -f gateway/Dockerfile -t eshop-gateway:bootstrap gateway
sh scripts/check-boundary.sh
```

The last command runs finite native and disposable Docker probes. All publication
is on temporary loopback ports. Cleanup removes test containers, networks, volumes,
TLS material and runtime data on success, failure, SIGINT or SIGTERM (SIGKILL cannot
be trapped). Images/build caches remain reusable. The helper uses `docker cp` with
disposable volumes, so a Docker daemon with a separate filesystem is supported.
Do not retain legacy logs, cookies, or comparator bodies as CI artifacts. No new
CI job or deployment integration is added.

The original default remains:

```sh
docker build -t harbour-eshop .
docker run --rm -p 8002:8002 harbour-eshop
```

## Configuration

| Environment | Contract / default |
|---|---|
| `PUBLIC_BIND` | Socket address, `0.0.0.0:8002` |
| `ADMIN_BIND` | Separate socket address, `0.0.0.0:8003`; never publish |
| `PUBLIC_TLS_CERT` / `PUBLIC_TLS_KEY` | Required readable matching PEM chain/key |
| `LEGACY_URL` | Required HTTPS origin only; no credentials, query, fragment or base path |
| `LEGACY_CA_FILE` | Required nonempty PEM trust store; chain and hostname checked |
| `ENABLED_OWNERS` | Comma-separated IDs, empty by default; unknown/duplicate IDs fail startup |
| `CONNECT_TIMEOUT_SECONDS` | TCP + TLS / public TLS deadline, 10 |
| `CLIENT_READ_TIMEOUT_SECONDS` | Absolute streamed request-body deadline, 120 |
| `UPSTREAM_TIMEOUT_SECONDS` | Response-header wait and separate absolute response-body deadline, 150 |
| `SHUTDOWN_TIMEOUT_SECONDS` | Maximum connection drain, 30 |
| `LOG_LEVEL` | `error`, `warn`, `info`, `debug`, `trace`; default `info` |

All configurable durations must be integers in 1..=300 seconds. Public HTTP
headers have a fixed 30-second deadline; admin headers have 3 seconds. Legacy
header/TLS and body windows are 30 and 120 seconds. Gateway deadlines are **new
edge behavior**, not perfect timing equivalence. Pre-header timeout yields 504;
other transport failures yield 502, including request-stream failures reported by
Hyper. Once headers have been sent, an interrupted or timed-out body terminates
the connection: no substitute response and no retry. There is no insecure TLS
bypass. Invalid local configuration and enabled IDs fail before listening;
upstream unavailability does not prevent liveness and makes readiness false.

For a native local run, supply externally prepared SAN-bearing public TLS files
and upstream trust, with a reachable HTTPS Harbour origin:

```sh
PUBLIC_BIND=127.0.0.1:8002 ADMIN_BIND=127.0.0.1:8003 \
PUBLIC_TLS_CERT=/absolute/tls/public.crt PUBLIC_TLS_KEY=/absolute/tls/public.key \
LEGACY_URL=https://localhost:18002 LEGACY_CA_FILE=/absolute/tls/ca.crt \
ENABLED_OWNERS= cargo run --locked --manifest-path gateway/Cargo.toml
```

Use a separately running legacy listener on `18002` for that example. `GET /live`
and `GET /ready` are plain HTTP on admin only. Readiness performs a verified HTTPS
`GET /hello` with a 3-second overall limit and requires 200 with exactly `Hello!`.
It does not establish DBF integrity. Public `/live` and `/ready` belong to Harbour.
SIGTERM/SIGINT marks readiness false, closes listeners, gracefully drains existing
connections up to the shutdown deadline, then cancels remaining tasks. It never
stops Harbour or touches its runtime files.

## Ownership And Transport

`dispatch::registrations()` is the sole ownership registration point. A future
independent slice adds a handler implementing `Handler` and one registration, with
an exact path or segment-bounded family and an exhaustive allowed-method list.
Use legacy domain names (account, shopping, cart), and keep that handler's domain
logic separate from these integration modules. Registration is not activation:
`ENABLED_OWNERS` must explicitly name it. Invalid/incomplete registrations,
duplicate IDs/methods, and overlapping paths are rejected, even if disabled.
An enabled owner rejects unlisted methods with 405/Allow rather than falling back.
All unmatched/disabled paths and all their methods remain Harbour-owned.

The adapter uses a fixed configured origin, a fresh verified HTTP/1.1 TLS connection
and exactly one `send_request` per request. No pooled stale-connection retry,
redirect following, decompression, or automatic replay is present, including for
mutating GET cart operations. Raw path/query, public Host, body, cookies, status,
relative/absolute Location, and separate Set-Cookie fields are preserved. Both
directions strip hop-by-hop fields and fields nominated by Connection. Streaming
body wrappers retain size hints and declared Content-Length. Chunked requests
stay chunked: legacy hbhttpd does not decode them and chunked login still fails
with 303 `login?err`, whereas a valid fixed-length login succeeds. Do not activate
if that characterized baseline changes.

Only HTTP/1 is supported; HTTP/2, WebSocket and protocol upgrades are not in scope.
Spoofable Forwarded, X-Forwarded-* and X-Request-ID are discarded. The gateway
generates X-Forwarded-For from the socket, X-Forwarded-Proto=https and a process-local
monotonic correlation ID. There is no trusted-proxy mode. The fixed upstream alone
controls the socket destination, not public Host or forwarding headers.

Gateway JSON logs contain ownership/route label, method, status, duration,
correlation ID and header/body transport outcomes, never raw URI/query, headers,
cookies, authorization or bodies. **This is not system-wide privacy:** unchanged
Harbour traces expose passwords, request bodies and cookie values. `/info` exposes
transport/request details: legacy sees the gateway socket peer, its upstream TLS
session, and generated metadata, not the original client's socket/TLS. It is not
byte-compared as an equivalent page. Restrict all legacy logs and `/info` access
through external operational policy if required; this bootstrap does not change
their application behavior.

Legacy sessions are process-local, have a sliding 600-second timeout, and issue
SESSID with `path=/`, without Secure, HttpOnly or SameSite on creation. Gateway
does not inspect/share them. Any Harbour restart invalidates authenticated sessions.
Cart schema is at `app/eshop.prg:62`; item schema is at `app/eshop.prg:73`.

## Explicit Local Composition

The following commands describe a deliberate new local composition, **not a
state-preserving switch of an existing running shop**. Use separate create,
network attachment and start steps. Supply a read-only `gateway-tls` volume with
only public.crt, public.key and ca.crt; provision a private `eshop-runtime` volume
with the full retained runtime and upstream TLS certificate/key before starting.
Certificates must include SANs matching the public hostname and `legacy`;
the unchanged entrypoint's generated CN-only localhost certificate is unsuitable
for upstream trust as `legacy`. Provisioning the volume contents is an external
prerequisite, not a production certificate generator in this repository.

```sh
docker network create --internal eshop-private
docker network create eshop-edge
docker create --name eshop-legacy --network eshop-private --network-alias legacy \
  -v eshop-runtime:/app harbour-eshop:boundary
# Populate the stopped container's /app with the retained runtime and correct TLS pair.
docker create --name eshop-gateway --network eshop-private \
  -p 127.0.0.1:8002:8002 -v gateway-tls:/tls:ro \
  -e PUBLIC_TLS_CERT=/tls/public.crt -e PUBLIC_TLS_KEY=/tls/public.key \
  -e LEGACY_URL=https://legacy:8002 -e LEGACY_CA_FILE=/tls/ca.crt \
  -e ENABLED_OWNERS= eshop-gateway:bootstrap
docker network connect eshop-edge eshop-gateway
docker start eshop-legacy
docker start eshop-gateway
```

Do not publish legacy 8002 or admin 8003. Harbour defaults to binding 0.0.0.0;
privacy comes from the internal network and absent host publication, not a source
change. Only Harbour mounts the full writable runtime; gateway never receives
legacy's key or `/app`. Image builds have no public side effects. Test CA material
is temporary and is not production provisioning.

## Activation And Rollback

Before public activation, externally identify and retain the exact running legacy
image and full current runtime location; arrange a maintenance window, a quiesced
backup, valid TLS/trust and the state-retention procedure. Pass the gates against
the intended environment. Keep `ENABLED_OWNERS` empty. Docker cannot modify an
existing container's published ports in place; recreating Harbour loses sessions.

1. Stop incoming traffic and quiesce/stop the current sole Harbour writer.
2. If runtime was in a container layer, `docker cp OLD:/app/. RETAINED_DIRECTORY`
   while stopped; preserve every runtime file and copy to the retained writable
   volume. Keep a restricted backup separate from the current volume.
3. Recreate the retained image as private `legacy` using that **current** runtime,
   install its SAN-bearing upstream pair while stopped, and start it alone.
4. Explicitly start gateway publication with appropriate public TLS and upstream
   CA; verify admin readiness, `/hello`, a fresh login, account and cart state.
   Replace sample loopback publication only through a deliberate external policy.

For rollback, stop gateway publication first. Quiesce and stop private Harbour if
recreation is needed. Retain the latest full runtime (not the activation backup),
ensure a certificate/key appropriate to the restored public hostname, and recreate
the exact retained legacy image with public 8002 and that current writable state.
Never run two Harbour writers against the directory, start a fresh seeded image,
or overwrite newer successful writes with an older backup. Expect old sessions to
be invalid; verify `/hello`, fresh login, retained account name and cart contents.
Keep backups/logs restricted because DBFs contain plaintext passwords.

Disabling a future owner is a separate action and requires its state contract to
permit legacy ownership. This bootstrap provides no reverse data migration.

## Validation And Limits

The disposable harness verifies 36 paired direct/proxied steps on separate identical
legacy instances with independent cookie jars: hello/root, protected redirects,
login/logout, register validation/retry/duplicate, account validation/edit, static
and missing assets, unknown routes, HEAD/OPTIONS, pagination and cart add/remove.
It compares status, Location, content type, cookie attributes and stable bodies;
only random SESSID values are normalized. Mutations never run twice on one instance.
It records /info differences and asserts the chunked-login failure baseline.

Controlled native verified-TLS probes exercise the actual `cargo run` entrypoint,
raw queries/Host, fixed framing, forwarding metadata, multiple cookies, hop headers,
absolute redirects, ambiguous GET/POST failure without replay, deadlines, truncated
and stalled response bodies, stalled client upload, graceful and forced drain,
invalid configuration/activation, mismatched public key, wrong upstream trust and
hostname, and safe logs. Docker probes exercise image entrypoints, delayed upstream,
outage/liveness/readiness recovery, private publication/mounts, restart session loss,
current DBF retention and rollback to the same legacy image/public listener with
fresh login and retained account/cart state.

These are local gates, not load tests, public security hardening, production trust
provisioning, zero-downtime switching or proof of data integrity. Independent owner
activation is exercised through focused dispatch tests, not a shipped demo route.
No application route is activated or implemented. Defer domain modules, persistence
interfaces, export/import, session replacement, static serving, rendering, workers,
queues, deployment-platform integration and generic migration tooling until an
assigned behavior needs them.

Implementation validation passed: locked native build, Rustfmt, warning-free
Clippy, five focused Rust tests, both image builds, the full boundary harness,
and the original `docker build -t harbour-eshop .` / published 8002 run smoke.
Test resources were removed. No legacy source, root Dockerfile/entrypoint, or
legacy workflow changes were made.

No LLVM coverage reporting tools are installed and none were installed for this
task. As an equivalent execution-coverage check, the Rust tests were built with
`RUSTFLAGS='-C instrument-coverage'` in a temporary target directory. Running the
same test binary with `--list` versus executing tests changed nonzero raw LLVM 22
instrumentation counters from 0 to 905 out of 3172. This verifies test execution
changes coverage; it is **not** a source-line coverage percentage (the counters
include instrumented dependencies). Production prerequisites and deferred
application work remain as described here; local success does not waive them.
