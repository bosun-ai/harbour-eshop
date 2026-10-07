# Bootstrap Verification

Executed locally against the checked-in locked crate and separate gateway image.
No shared environment or infrastructure was changed.

## Changes

- Standalone Rust ingress, typed configuration, authenticated TLS upstream and
  independent registration/activation dispatcher; initial registry stays empty.
- Separate non-root image, explicit foreground Docker activation, sample config,
  operator/rollback guide and separate CI job; legacy default job remains intact.
- Only root integration edits are README, target ignores and the additional CI job.
  Harbour source, templates, assets, DBFs, root Dockerfile and entrypoint are unchanged.

## Checks And Results

- `cargo build --locked --release`, `cargo test --locked` (3 integration tests),
  `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`: passed
  using `--manifest-path gateway/Cargo.toml` and recorded Rust 1.97.1.
- Root Harbour image and gateway-context image builds: passed. Default image
  entrypoint remains `docker-entrypoint.sh`; gateway image runs only its binary.
- `migration/verify.py` with both images: passed through native CLI, real HTTPS
  listeners, real image entrypoints and real activation script, not mocked hosting.
- Invalid config/activation, missing or mismatched key, wrong SAN/CA, unavailable
  upstream, readiness recovery, trusted/untrusted metadata and trace-level secret
  redaction: passed. Diagnostic failures reveal categories, not configuration values.
- Raw query encoding/order, Host/TLS separation, repeated Set-Cookie, unchanged
  known-length body, absolute Location, gzip pass-through and hop removal: passed.
  Chunked requests rejected; invalid/ambiguous framing rejected before submission.
- Before/after-submission disconnects and response timeout issue exactly one
  upstream connection for GET mutations. Partial-stream errors terminate delivery;
  upload idle timeout, cancellation of a huge stream and slow TLS/header tests pass.
- Saturated public connection cap preserves management access. Graceful drain
  completes bounded work; shorter drain deadline cancels outstanding work. The
  foreground activation script drains ingress and stops the sole Harbour writer.
- Independent disposable direct/proxy corpora match status, Location, cookie
  attributes, content type and stable bodies: root/unknown/static paths, unsupported
  methods, failed login, failed-form retention, registration, login/logout, account
  edits, pagination and cart add/delete. Only opaque generated SESSID values are
  normalized. `/info` forwarding/topology differences are checked separately.
- Deployment inspection confirms private Harbour has no published port, gateway
  has no `/app` mount and publishes no management port. Only ingress joins an
  additional public-capable network; Harbour remains internal-only.
- Gateway replacement retains the running writer, session and cart total 53.34.
  Quiesced fallback recreates direct Harbour with the same runtime volume;
  DBF SHA-256 hashes match, re-login restores account `Updated` and total 53.34.
  Old session invalidation on Harbour restart is observed, as expected.
- Explicit activation succeeds, a second activation against an owned runtime is
  refused, interrupt cleanup removes its containers/networks and retains state.
- Coverage equivalence without installing tools: compiled integration tests with
  `RUSTFLAGS='-C instrument-coverage'` into a temporary target. Compared the same
  test executable's `--list` baseline and full test LLVM raw v10 profiles: nonzero
  execution counters increased from 0 to 763 of 3417. This proves tests execute
  previously unexecuted code; it is not a source-line coverage percentage. LLVM
  header/record layout was checked against upstream's release/22.x definition.

Docker in this environment uses a daemon-local filesystem, so local bind paths
are not visible. The available equivalent is disposable named volumes populated
with `docker cp`, supported explicitly by the activation script. Real writable
runtime retention, read-only gateway inputs and exclusive-writer checks were
executed using that mode. This is not an unverified bind-path deployment claim.
Temporary test containers, networks and volumes are cleaned by the harness.

### Backpressure Deadline Repair

- The public connection independently enforces the legacy response deadline,
  including during shutdown drain; expiry drops the stream and upstream driver.
- The no-read regression occupies all 128 public permits, keeps clients open,
  verifies upstream cancellation within 1.5 seconds for a 700 ms deadline, and
  verifies a new public request succeeds before any client is closed.
- The probe failed against the original binary and passed three repetitions each
  with and without drain against the repair. Native transport verification,
  locked release build/tests, formatting, Clippy and shell syntax passed.
  Python trace coverage increased from 0 to 37 executed lines in the new probe.

## External Prerequisites

Production activation still requires operator-owned public TLS, an upstream SAN
and trust root, private networking, exclusive persistent current runtime, recorded
image identity and rehearsed public-name/certificate fallback. Build requires
Rust/Docker/Python/openssl and access to dependencies and base images. The script
does not discover secrets, initialize live storage or deploy shared infrastructure.
Harbour trace logs still expose passwords/cookies; gateway redaction does not fix
that legacy behavior. Session expiry remains process-local, 600 seconds.

## Deferred Application Work

No application routes, authentication, session sharing, rendering, DBF ownership,
workers or business logic are migrated. Future slices use the registry/handler
extension point, with a separate activation change and their own application
prerequisites. Unknown-length request compatibility, HTTP/2, upgrades and broader
protocol support require explicit future decisions rather than transparency claims.
