# Opt-In HTTPS Boundary

This is transport plumbing, not an application migration. `app/`, the root
Dockerfile, entrypoint, and standalone commands remain unchanged. Merge does not
activate this composition or any handler. The shipped registry is empty.

## Build And Run

Requires Rust 1.97, Docker, Docker Compose v2, Python 3, and OpenSSL. No Harbour
installation is needed for the container path. From the repository root:

```sh
cargo build --locked --manifest-path gateway/Cargo.toml
cargo test --locked --manifest-path gateway/Cargo.toml
cargo fmt --manifest-path gateway/Cargo.toml -- --check
cargo clippy --locked --manifest-path gateway/Cargo.toml --all-targets -- -D warnings
docker build -t harbour-eshop:bootstrap .
docker build -f gateway/Dockerfile -t eshop-gateway:bootstrap .
```

Prepare local throwaway SAN-bearing **leaf** certificates before startup. The
public certificate covers `localhost`; the distinct upstream certificate covers
`legacy`. `CA:FALSE` matters: webpki rejects a CA certificate used as a leaf.
For real ingress supply appropriate signed certificates, chains and upstream
trust instead; never disable hostname verification.

```sh
mkdir -p local/tls
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj /CN=localhost \
  -addext 'subjectAltName=DNS:localhost,IP:127.0.0.1' \
  -addext 'basicConstraints=critical,CA:FALSE' \
  -keyout local/tls/public.key -out local/tls/public.crt
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj /CN=legacy \
  -addext 'subjectAltName=DNS:legacy' \
  -addext 'basicConstraints=critical,CA:FALSE' \
  -keyout local/tls/legacy.key -out local/tls/legacy.crt
# Only for these disposable local keys: gateway runs as UID 65534.
chmod 644 local/tls/public.key
docker compose -p eshop-bootstrap -f compose.bootstrap.yml build
docker compose -p eshop-bootstrap -f compose.bootstrap.yml up -d
curl --cacert local/tls/public.crt https://localhost:8002/hello
python3 scripts/verify-bootstrap.py --project eshop-bootstrap
```

The verifier requires the cargo-built debug binary for its local raw TLS peer.
It creates independent fresh direct/proxied volumes and cookie jars, then removes
its containers, volumes, network, and temporary certificates. It only inspects
and probes `/hello` on the supplied composition, never writes its data. Without
Compose, omit `--project`: equivalent Docker CLI fixtures exercise both images
and the local binary, but do not validate Compose parsing. A remote Docker daemon
is supported by copying fixture certificates into volumes rather than binding
local paths. The documented Compose TLS bind mounts require a local daemon or
operator-provisioned files on the daemon host.

Stop without destroying runtime records:

```sh
docker compose -p eshop-bootstrap -f compose.bootstrap.yml down
```

Do not use `down -v` for retained data. Harbour alone owns the runtime volume;
there is no gateway `/app` mount. Only gateway application port 8002 is published.
The upstream network is internal. Admin port 8003 is private, plain HTTP; a
private monitor must GET `/live` or `/ready`. Readiness performs a verified,
bounded `/hello` probe requiring status 200 and exactly `Hello!`; it never
restarts Harbour. Startup does not wait for upstream readiness. No automatic
healthcheck restart policy or orchestrator is introduced.

## Configuration

| Environment | Default / validation |
|---|---|
| `GATEWAY_PUBLIC_BIND` | `0.0.0.0:8002`, socket address |
| `GATEWAY_ADMIN_BIND` | `127.0.0.1:8003`; composition uses private `0.0.0.0:8003` |
| `GATEWAY_TLS_CERT`, `GATEWAY_TLS_KEY` | Required readable PEM public chain/key, matching pair |
| `GATEWAY_LEGACY_URL` | Required HTTPS origin; no credentials, query, fragment, or base path |
| `GATEWAY_LEGACY_CA` | Required PEM trust bundle; gateway never receives Harbour's key |
| `GATEWAY_ENABLED_SLICES` | Empty; comma-separated registered IDs, unknown IDs fail startup |
| `GATEWAY_CONNECT_SECONDS` | 5; includes DNS, TCP, TLS, HTTP handshake; also public TLS handshake |
| `GATEWAY_HEADER_SECONDS` | 30; includes sending the request body through receipt of upstream headers |
| `GATEWAY_IDLE_SECONDS` | 30; body frame inactivity and public/admin header-read bound |
| `GATEWAY_DRAIN_SECONDS` | 10; SIGTERM/SIGINT stop listeners, drain, then abort remaining tasks |
| `GATEWAY_LOG_LEVEL` | `info`; validated tracing level (`off`, `error`, `warn`, `info`, `debug`, `trace`) |

Timeouts are integer seconds in 1..86400. Slow uploads may hit the header deadline;
slow streams hit the idle deadline. These are deliberate compatibility limits,
not claims of arbitrary streaming transparency. HTTP/1.1 only; no upgrades or
HTTP/2. Each legacy request uses exactly one connection and is never retried,
including mutation GETs. There is no redirect following, decompression, cache,
session handling, forwarding-header injection, or externally supplied log ID.

## Compatibility And Dispatch

Methods, raw path/query, known-length bodies, incoming Host, cookies, repeated
Set-Cookie, status, Location, content type and body are forwarded. Hop-by-hop
headers and Connection-nominated headers are removed on both sides; Hyper
manages framing. Header case/order, Connection and transfer framing may change.
Content-Encoding is preserved without decoding. `/info` exposes the gateway peer
and connection variables, so its rendered bytes are not identical. No added
Forwarded/X-Forwarded/correlation header changes its diagnostics.

Harbour does not decode chunked request bodies. Gateway rejects Transfer-Encoding
requests with 501 **before contacting Harbour**, rather than making chunked writes
work by dechunking. This is an explicit difference from Harbour's missing-fields
redirect on chunked registration. Known-length `Expect: 100-continue` is terminated
locally by Hyper; Expect is not forwarded. Informational responses are not
preserved end-to-end. Invalid HTTP framing may be rejected by Hyper before
dispatch. Ordinary unsupported methods and unknown paths still fall back to
Harbour (HEAD/OPTIONS `/hello` return 501; missing paths return 404).

Pre-header upstream connection/TLS/protocol failures return 502; deadlines return
504. Once headers start, body errors terminate the stream, without a replacement
response or replay. JSON stdout logs use locally generated request IDs, bounded
method labels (extension methods become `OTHER`), owner, status and header-time
duration; failure categories are sanitized. They contain no paths, queries,
bodies, credentials, cookie values or serialized requests. This claim does not
cover Harbour's unchanged tracing. Header duration is not total stream duration.

Future independent handlers use `Request<Body> -> async Response<Body>`, with
streaming standard HTTP bodies, and one `Registration` in `main.rs`. Registration
declares a stable ID, methods, exact path or boundary-matched subtree, and handler.
No listener, proxy, network, or Harbour edit is needed. Domain logic belongs in
that handler's domain module, not in integration files; use legacy terms such as
catalogue, items and carts. No empty domain abstraction is introduced here.
Only active registrations participate in ownership conflict checks. Disabled,
unmatched and method-mismatched requests fall back, not gateway 404/405.
Removing an enabled ID is separate operator rollback for an independent slice.

## Activation And Rollback

Existing `docker build -t harbour-eshop .` and `docker run --rm -p 8002:8002
harbour-eshop` remain default. Selecting this composition is explicit operator
activation, not a production deployment prescription. Before switching ingress,
retain the exact legacy image/runtime files, provision real certificates, pass
compatibility gates, and test the operator's switch-back mechanism. Never run
two Harbour writers on the same DBFs. The repository provides no atomic port
switch; expect a brief interruption where publications require recreation.

For gateway-only rollback restore direct ingress to **the same running Harbour
process** through an operator-tested mechanism. Local verifier uses a test-only
loopback publication to prove its session/cart survive gateway shutdown. That
publication is not a deployment default. If Harbour must be recreated, stop the
old writer first and reuse its retained runtime volume (including executable,
templates, TLS files and DBFs); never fall back to image-seeded tables. Its
process-local sessions are lost, so users must log in again even when account
and cart records survive. Upstream sessions renew for 600 seconds, use opaque
SESSID cookies with path=/, deletion Max-Age=0, and lack Secure/HttpOnly/SameSite.
This bootstrap does not harden or translate them.

## Release Checks And Deferred Work

Focused Rust tests cover configuration, activation/inactivity, overlaps, method
and path boundaries, hop headers, cookie multiplicity, idle expiry and driver
cancellation. The verifier compares complete stable journey bodies and exact
redirects/content types/cookie attributes and multiplicity (only SESSID token
values are normalized). It exercises registration, failed-form retention, login,
pagination, logout, CSS, encoded requests and two cart adds totaling 53.34. It
also checks TLS hostname/trust failures, missing/invalid startup config, startup
ordering, readiness loss/recovery, restart data/session behavior, gateway-only
rollback, no replay of mutation GETs, raw framing/header preservation, disconnects,
partial-stream failure, header timeout, chunked rejection, Expect and log safety.

CI adds these checks without changing the existing legacy smoke job. Deployment
certificate management, trusted ingress policy, atomic switching, durable-volume
operations, application handlers, datastore ownership, sessions, exports/imports,
workers and application migration remain deferred. Catalogue schema is at
`app/eshop.prg:73`; carts at `app/eshop.prg:62`.

### Implementation Run Evidence

In the implementation environment, both Docker image builds, locked Cargo build
and tests (4 passed), formatting, Clippy with warnings denied, Python compilation,
and the full disposable verifier passed. An equivalent Docker CLI composition
with an internal upstream network plus a public gateway network also answered
`Hello!`; inspection confirmed exclusive Harbour runtime ownership. No test
fixtures remain. The root Dockerfile, entrypoint, application and existing smoke
workflow have no diff.

Docker Compose v2 is absent here and the daemon is remote, so actual Compose
build/up/`--project` commands were not executed locally. Both YAML files parse;
the equivalent Docker CLI topology was exercised. The added CI job exercises the
exact Compose commands on a local daemon; its hosted result is not claimed here.
Harbour's non-container build is also unavailable; the real Docker compiler and
runtime entrypoints were used instead. No tools were installed.

Rust coverage instrumentation was run against an identical test binary with all
tests skipped and then all tests enabled. Using LLVM 22 raw-profile v10 counter
records, non-test `eshop_gateway` executed counters increased from **0 to 301 of
736** (40.9%). This is counter coverage, not a line/branch percentage or complete
boundary coverage. LLVM reporting executables are absent locally; CI emits the
standard LLVM source report. Transport/lifecycle behavior is additionally covered
by black-box checks, not those unit-test counters. Resource exhaustion, a real
ingress switch, signed production certificate provisioning and deployment-scale
load are not validated by this bootstrap.
