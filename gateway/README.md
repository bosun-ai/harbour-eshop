# Opt-In Bootstrap Gateway

This is an all-proxy transport experiment, not a feature migration. The default
root Docker image, command, Harbour sources, templates and DBFs are unchanged.
Merging this directory, running CI, or registering a slice does **not** activate
the gateway. Harbour is the only owner of application data and sessions.

## Build And Check

Rust **1.97.1** is used by CI and the separate image. OpenSSL is needed by test
fixtures, not by the gateway executable. Python 3 and a local Docker daemon are
needed by the black-box harness. No Harbour host installation is required.

```sh
cargo build --locked --manifest-path gateway/Cargo.toml
cargo fmt --manifest-path gateway/Cargo.toml --all -- --check
cargo clippy --locked --manifest-path gateway/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path gateway/Cargo.toml --all-targets
docker build -t harbour-eshop:local .
docker build -f gateway/Dockerfile -t eshop-gateway:local gateway
python3 gateway/verify.py --legacy-image harbour-eshop:local --gateway-image eshop-gateway:local --native
```

The harness uses disposable, separate identically seeded legacy state volumes,
loopback-only publications and a private temporary network. Files are transferred
with `docker cp` (remote daemons cannot see host temporary bind paths). It cleans
up containers, volumes, network and certificate files even on assertion failure.
It uses STOP/CONT rather than a cgroup freezer for outage/recovery. Test public
requests accept disposable self-signed certificates; gateway upstream verification
always checks trust and hostname. Gateway mounts only its read-only TLS volume,
never the legacy runtime volume or upstream private key.

## Configuration

All settings are environment variables. Invalid configuration/selection rejects
startup before public binding. PEM certificate/key matching is verified by Rustls.

| Variable | Policy / Default |
| --- | --- |
| `PUBLIC_CERT`, `PUBLIC_KEY` | Required readable matching PEM files, provisioned externally |
| `LEGACY_UPSTREAM` | Required HTTPS origin only, e.g. `https://legacy:8002`; no credentials, query, fragment or application path |
| `LEGACY_TRUST` | Required PEM trust bundle; no insecure verification switch |
| `PUBLIC_BIND` | `0.0.0.0:8002` |
| `MANAGEMENT_BIND` | `127.0.0.1:9000` for host use; explicitly set `0.0.0.0:9000` in a container, never publish in operator deployments |
| `ENABLED_SLICES` | Empty; comma-separated stable IDs; unknown/duplicate IDs reject startup |
| `LOG_LEVEL` | `info`; accepts `off`, `error`, `warn`, `info`, `debug`, `trace` |
| `CONNECT_SECONDS` | 5 per connect/TLS phase; also total readiness deadline |
| `HEADER_SECONDS` | 30, matching legacy's existing header wait |
| `REQUEST_BODY_SECONDS` | 120 total body deadline, matching legacy's body wait |
| `UPSTREAM_RESPONSE_SECONDS` | 150 to response headers, includes known-length upload |
| `BODY_IDLE_SECONDS` | 30 inactivity limit on body streams |
| `SHUTDOWN_SECONDS` | 10 drain deadline; unfinished connections are dropped |

Every timeout must be an integer from 1 to 300 seconds. Public TLS is HTTP/1.1
only. Management offers plain HTTP `GET /live` and `GET /ready`, not public health
routes. `/hello` always goes to Harbour. Readiness expects strict-TLS upstream
`200` and exactly `Hello!` within the deadline; liveness is independent of legacy.
Health requests are not application ownership registrations.

For host execution, supply certificates first, then run:

```sh
export PUBLIC_CERT=/absolute/path/public.crt PUBLIC_KEY=/absolute/path/public.key
export LEGACY_TRUST=/absolute/path/legacy-ca.crt LEGACY_UPSTREAM=https://localhost:18002
export PUBLIC_BIND=127.0.0.1:8002 MANAGEMENT_BIND=127.0.0.1:9000
cargo run --locked --manifest-path gateway/Cargo.toml --bin eshop-gateway
```

The upstream certificate must have a SAN matching the URL hostname. The legacy
entrypoint's generated CN-only certificate is insufficient. Pre-supply
`certificate.crt` and `private.key` in its retained `/app` volume; the existing
entrypoint retains them. Provision public and upstream keys independently. The
gateway image runs UID/GID 65532; externally supplied files must be readable by
that user without giving it legacy keys or writable application mounts.

## Ownership Boundary

`ownership::registrations()` is deliberately empty. A future slice adds its own
implementation and one `Registration` there. It uses `Request<Body>`, a local
`Context`, and `Response<Body>`; application logic belongs in the slice, not in
the HTTP adapter or dispatcher. No session, DBF, authentication or datastore
abstraction is introduced by this bootstrap.

Registration defines its entire method policy; unsupported methods cannot fall
back to legacy within selected ownership. Selection is a separate allow-list
operation. Exact paths are literal; families match only their root and `/`
segments. Raw encoded separators are not decoded or normalized. Duplicate IDs,
empty/invalid descriptors and overlaps (even disabled ones) reject startup.
Unregistered and unselected paths, including unknown paths, retain legacy behavior.
Two **test-only** `/hello` and `/info` handlers verify each independently selected,
both selected and neither selected across eight methods and ten raw paths. These
handlers do not ship as production application behavior.

## Transport And Compatibility

Each request opens one strict-TLS HTTP/1.1 upstream connection. There is no pool,
retry, redirect following, decompression or HTML rewriting. This matters because
legacy GET cart operations mutate data. Raw path/query, method, original Host,
opaque cookies, body bytes, end-to-end headers, status, relative Location and
repeated Set-Cookie are retained. Hop-by-hop headers and Connection-nominated
fields are removed. Inbound `Forwarded` and `X-Forwarded-*` are discarded: direct
clients cannot assert identity. No forwarded header or request ID is injected.

Known Content-Length bodies stream. **Explicit compatibility exception:** bodies
without Content-Length are decoded and buffered up to **1 MiB**, under body/idle
deadlines, then sent with Content-Length; Harbour never receives chunked framing.
Above the bound returns 413. Tests verify that legacy chunked login fails while
gateway chunked login succeeds. This is not claimed as byte-equivalent behavior;
operators must accept this bounded normalization before activation. Trailers are
not forwarded. Malformed HTTP framing may be rejected by Hyper before dispatch.

Before response headers, connect and TLS failures return 502, deadlines return
504, and protocol/disconnect errors return 502. A known-length upload failing
inside Hyper can surface as a protocol 502; an unknown-length body failing during
buffering surfaces as a deadline 504. After headers, stream failure/idle timeout
terminates the response, never invents a replacement response or replays a request.
SIGINT/SIGTERM stops acceptance, drains active connections and aborts at the
deadline; it never writes `.uhttpd.stop` or controls Harbour.

Logs contain local sequence ID, owner, an allow-listed method (extensions become
OTHER), status, time to response headers and a sanitized failure category. No
path/query, header, cookie, credential or body is logged. `error`/`warn` emit only
failures; `info`/`debug`/`trace` also emit completions. Stream errors after response
headers do not have separate structured logs in this bootstrap. **Legacy tracing
still exposes request headers and bodies**; gateway redaction does not fix that.

The real harness compares stable bytes and selected headers for login, redirects,
registration errors/retained values, CSS/conditional retrieval, missing paths,
unsupported methods, pagination, account retry/update, both registration-session
and login-session carts, and logout. Session IDs and Date are dynamic. `/info`
is compared with only `REMOTE_ADDR`, `REMOTE_HOST`, `REMOTE_PORT`, `SERVER_ADDR`,
`SERVER_PORT` and `HTTP_CONNECTION` values excepted. TLS metadata may need further
characterization with different client/provider configurations; the local corpus
does not exempt it. Existing duplicate cart rows and deletion quirks are preserved,
not replaced by idealized cart contracts.

## Activation And Rollback

1. Identify and retain the actual legacy state volume/directory, certificates and
   image provenance. Never use a new seeded container as fallback or run two
   Harbour processes over the same DBFs.
2. Provision separate public TLS and upstream SAN/trust inputs. Run one Harbour
   writer on a private network **without publishing its port**; its existing
   `0.0.0.0:8002` listener is unchanged. Do not expose management to public traffic.
3. Start the separate gateway executable/image with empty `ENABLED_SLICES`, only
   gateway TLS/trust mounted read-only. Verify private readiness and compatibility
   before publishing public port 8002. The gateway does not launch Harbour.
4. Publish gateway traffic only through an explicit operator action. There is no
   repository-established production topology or zero-downtime cutover promise.
5. Roll back by removing gateway traffic, stopping the old legacy container if
   publication must change, and restoring direct Harbour publication with the
   **same retained state and certificate inputs**. Do not start the replacement
   until the previous Harbour writer is stopped. No data conversion is needed.

Changing container publication may restart Harbour. Its sessions are process-local
with a 600-second sliding expiry; restart loses authentication. Existing `SESSID`
cookies have `path=/`, no Secure/HttpOnly/SameSite on creation; Rust keeps them
opaque and does not promise shared sessions or restart-transparent authentication.

## Gaps And Deferred Work

The bounded checks are not a browser-wide, concurrency/load, hostile-client or
production ingress assessment. Persistent provisioning, DNS/firewall ownership,
certificate rotation, safe legacy logging, secret/backup controls and accepting
the chunked-request exception are external operator prerequisites. Native Harbour
compilation needs Harbour installed; the real Docker build exercises hbmk2 instead.
Coverage-instrumented Rust tests can be run with `RUSTFLAGS='-C instrument-coverage'`
and `LLVM_PROFILE_FILE`; profile aggregation requires compatible LLVM tools, which
are not a gateway/runtime prerequisite.

Datastore/transaction choices, typed domain identifiers, session replacement,
credentials, rendering, mutable feature ownership and its export/import/rollback
remain future application work. This bootstrap adds none of them.
