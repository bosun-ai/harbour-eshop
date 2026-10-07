# Opt-In Hosting Bootstrap

This binary is an HTTP/1.1 HTTPS transport adapter, not an application migration.
Harbour still owns login, register, account, shopping, cart, templates, static
files, cookies, sessions, DBF/CDX and all runtime files. Rust never opens `/app`.
There is no domain layer because no domain behavior has moved. Integration code
is separated into configuration, ingress, dispatch, proxy and operations modules.

**Merge/build is not activation.** The root Dockerfile, entrypoint, application and
legacy workflow are unchanged. The compiled ownership list and activation list
are empty; any nonempty `GW_ACTIVE_FAMILIES` fails startup. Future ownership units
must be implemented and registered within the gateway, never by editing Harbour.
Before supporting nonempty activation, add complete path/method definitions and
startup conflict validation; merely compiling a future handler must not select it.
There is no plugin loader, partial application handler or hot reload.

## Build And Run

Prerequisites: Docker CLI and reachable daemon, OpenSSL, Python 3, the Rust
toolchain in `rust-toolchain.toml`, registry/package network access and free local
ports 18002–18006. Do not run concurrent experiments using the fixed
`eshop-bootstrap` resource prefix. No Compose, deployment or shared infrastructure
changes are needed. Image builds fetch their normal build dependencies.

```sh
cargo build --locked --manifest-path gateway/Cargo.toml
cargo fmt --manifest-path gateway/Cargo.toml --check
cargo clippy --locked --manifest-path gateway/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path gateway/Cargo.toml
docker build -f gateway/Dockerfile -t eshop-gateway:bootstrap gateway
./gateway/scripts/local.sh verify
./gateway/scripts/local.sh up
./gateway/scripts/local.sh down
```

Use `cd gateway` when selecting the pinned toolchain through rustup: rustup finds
toolchain files from the working directory, not from `--manifest-path`. The Docker
build and new CI already do this. `up` assumes both images are built; `verify`
builds both and cleans up on success/failure. `down` deletes **experimental** data,
containers, network, certificate volume and scratch keys. It is not a production
rollback or backup command. Do not store real data in these resources.

`up` stages SAN certificates with `docker cp`, so client filesystem bind mounts
are unnecessary even for a remote daemon. Harbour has no published port. Only
the gateway publishes loopback 18002. Its private operations listener is loopback
9000 inside the gateway container. Its certificate mount is read-only and separate
from Harbour's complete runtime volume. The nonroot gateway image directly runs
`eshop-gateway serve`. Check it with:

```sh
curl --cacert gateway/.local/ca.crt https://localhost:18002/hello
docker exec eshop-bootstrap-gateway eshop-gateway check-ready
```

For source execution, export `GW_*` using `config/example.env` as a reference,
substituting real local certificate paths, free bind ports and a reachable fixed
HTTPS Harbour origin, then:

```sh
cargo run --locked --manifest-path gateway/Cargo.toml --bin eshop-gateway -- serve
```

The example env file describes container paths; it is not directly usable on the
host. `serve` validates certificate/key pairing, trust parsing, origin, addresses,
activation and limits before binding. A reachable but untrusted upstream leaves
liveness up, readiness down and application requests returning 502; it never
disables verification. `/ready` performs one verified `GET /hello` and requires
exactly `200`/`Hello!`, with no cookies or redirects. `/live` checks the gateway.

## Transport Contract

- Preserve raw encoded path/query, incoming Host, methods, status, relative
  Location, content type, body bytes and repeated end-to-end headers. No client
  header selects the upstream destination or establishes trusted identity.
- Open one TLS connection/request attempt. Never retry, including cart-mutating
  GET. No pooling, cookie jar, redirect following, caching, decompression or HTML
  rewriting. Streaming bodies apply backpressure; request bodies are not buffered.
- Remove hop-by-hop fields and Connection-nominated fields. Reject Connection
  nominations of Host/framing fields. Preserve chunked framing without making it
  a successful Harbour form: chunked registration still redirects to `?err=1`.
- Reject mixed Transfer-Encoding/Content-Length and duplicate Content-Length
  before Hyper can discard their evidence. Only plain `chunked` transfer coding
  is accepted. Reject absolute-form destinations, CONNECT and upgrades. No HTTP/2
  or WebSocket support. These are explicit protocol restrictions.
- Bound the initial header block to 32 KiB, close the public connection after one
  exchange and cap concurrent public/operations connections together at 256.
  Over-cap connections close rather than queue indefinitely.
- Defaults: 5 seconds connect/TLS; 30 seconds initial headers, upstream response
  headers and per-body/downstream-write idle; 120 seconds total exchange; 30 seconds drain.
  `GW_{CONNECT,IDLE,TOTAL,DRAIN}_SECONDS` accepts 1–3600; connect/idle cannot exceed
  total. These are new operational limits, not promises about legacy behavior.
  The initial client headers have a separate idle bound, before exchange timing.
  Request-body idle timing starts on the first upload poll after upstream setup;
  connection setup still counts toward the independent total deadline.
  Response-header idle timing starts after upload completion; early upstream
  responses remain supported. Public total and socket-write idle deadlines run
  independently of body polling, also during shutdown drain. Closing an exchange
  cancels its upstream driver. Explicit upstream ports must parse as u16; only an
  omitted port defaults to 443.
- SIGTERM/SIGINT closes listeners, signals active connections to finish, then
  aborts remaining tasks at the drain limit. Rust never writes a Harbour stop file.
- Gateway-generated errors say delivery outcome may be unknown. An error after
  response headers closes/truncates the response, rather than inventing an
  application failure status. Clients must not automatically replay mutations.

JSON request logs contain process-local correlation sequence, actual transport
peer, method, `legacy` owner, status, time-to-headers and sanitized error category.
No correlation or forwarded header is injected. Logs exclude URIs, queries,
cookies, bodies, credentials and raw upstream errors. IDs restart with the process;
body completion errors currently manifest as truncated responses, not a second
completion log. Harbour logs/traces are unchanged and require separate access and
collection restrictions.

Allowed compatibility differences are Date, connection/framing headers and
`/info` socket/transport variables. `/info` is not rewritten or byte-compared.
Independent session values differ; cookie attributes and same-process continuity
are asserted. Stable application bodies are compared exactly, not normalized.

## Activation And Rollback

Public activation is an external, explicitly approved operational action. Before
it, require approved diagnostic/protocol/timeout differences, verified public
certificates, upstream SAN/CA trust, retained legacy image digest and a rehearsed
handoff of **actual current runtime data**, not the shipped DBF seeds.

1. Verify a candidate gateway on a separate port against private Harbour.
2. Drain/release the old public port 8002. Docker mappings cannot change in place;
   this repository does not establish an ingress or zero-restart handoff.
3. If recreating Harbour privately, stop its sole writer first and retain the
   complete runtime state. A whole `/app` mount must contain the executable and
   assets as well as DBFs/CDXs. Prestage a verified private certificate at
   `certificate.crt`/`private.key`. Expect lost sessions on Harbour restart.
4. Explicitly publish only the gateway on host 8002 and verify hello, session,
   login, account and cart. Harbour remains the sole writer.

Rollback: drain/stop the gateway, release 8002, stop the private Harbour writer,
then run the retained legacy image publicly with **current** runtime records and
the expected public certificate/trust. Verify login, account and retained cart;
expect reauthentication after restarting Harbour. Never restore an old snapshot
as ordinary hosting rollback. Removing a future dispatch entry cannot bypass a
failed gateway, and mutable Rust data ownership rollback is outside this slice.

## Verification Results And Gaps

Local verification passed Rust build/format/clippy (`-D warnings`), five focused
unit tests, both actual Docker builds, `verify`, `up`, `down`, source `serve` and
SIGTERM. The additive CI invokes the same checks without activation/deployment.
The legacy smoke workflow remains unchanged.

The disposable corpus passed forms/redirects, unknown routes, HEAD/OPTIONS 501,
CSS/missing CSS, conditional static responses, encoded query, registration
validation/retry/duplicate, failed/successful login, protected navigation,
account retry/edit, pagination, two adds totaling 53.34, deletion and logout.
It uses separate seeded runtimes for differential mutations. Login is performed
before composite cart lookups: newly registered legacy sessions hold an unpadded
identity until login; this observed legacy behavior is preserved, not repaired.

Also passed: cookies crossing both directions against one Harbour process,
gateway restart retaining sessions, Harbour restart losing sessions but retaining
records, original Host/diagnostic header reflection, chunked failure, ambiguous
framing rejection, invalid config, wrong CA/name, private upstream/no gateway
`/app` mount, readiness failure with live gateway on outage, dropped/stalled
cart-mutating GET with exactly one upstream attempt, body-idle truncation,
SIGTERM draining an active response, and rollback using current runtime volume.
Review regressions also passed active slow uploads, idle uploads, early responses,
complete and streaming uploads after TLS setup longer than the body-idle limit,
post-upload header timeout, active-body total timeout, invalid/omitted upstream
ports (including a verified 443 connection), a nonreading 64 MiB response client,
and 256 nonreading clients followed by permit/public/health recovery. Socket state
and upstream closures are checked without draining those clients' response data.
Containers, network, volumes and scratch certificates were removed afterward.

Coverage-instrumented Rust tests ran and generated differing profiles from the
same binary with tests skipped. No percentage/line report is claimed: LLVM report
tools are absent, and no tooling was installed. Remaining gaps: exhaustive
slowloris/load/resource-cap tests, all timeout permutations (including DNS stalls),
forced drain expiry under large streaming uploads, complete protocol fuzzing,
production certificate lifecycle and real mutable-state handoff. The source-run
probe exercised listener/liveness/shutdown; HTTPS application compatibility ran
against the container entrypoint. No shared/public deployment was exercised.

Build provenance from this run (locally built images have no registry RepoDigest):

| Artifact | Identity |
| --- | --- |
| Rust | 1.97.1, commit `8bab26f4f68e0e26f0bb7960be334d5b520ea452` |
| Harbour source | `529b0d42939610a13da1572cd7861da6f9fa2d47` (existing Dockerfile) |
| Legacy image | `sha256:b799038836231ad88ed6a765c498bb32a693797a6b76e06e63145e299bfb09a4` |
| Gateway image | `sha256:1aef68dca5098b352ac08941a2b1398bcc52603ab173c9b38288b665dec6d18a` |
| Rust base RepoDigest | `rust@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97` |
| Debian base RepoDigest | `debian@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587` |

Cargo dependencies are locked; base tags and OS packages remain mutable. `verify`
prints image IDs/RepoDigests each run rather than promising identical images.

Deferred application work: session translation, domain/repository interfaces,
identity/money types, DBF access, templates, password migration, target storage,
workers/queues, shared telemetry, ownership-unit implementation/activation,
concurrency reconciliation, backups/exports and data cutover. None is needed by
the all-proxy hosting boundary.
