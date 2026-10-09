# Harbour eShop

[![build](https://github.com/bosun-ai/harbour-eshop/actions/workflows/build.yml/badge.svg)](https://github.com/bosun-ai/harbour-eshop/actions/workflows/build.yml)

A small legacy-style web shop written in [Harbour](https://github.com/harbour/core), the open-source successor of the Clipper/xBase 4GL. It is a sample application for [Bosun](https://bosun.ai) migration demos: real 4GL-era code with HTTP in, DBF files out, and business logic mixed in with rendering, just like most legacy code.

## Where the code comes from

The application in [`app/`](app/) comes from the official Harbour repository, with one mount-table integration for the default-disabled subprocess boundary:

- Source: [`harbour/core` → `contrib/hbhttpd/tests/`](https://github.com/harbour/core/tree/529b0d42939610a13da1572cd7861da6f9fa2d47/contrib/hbhttpd/tests)
- Pinned commit: [`529b0d4`](https://github.com/harbour/core/commit/529b0d42939610a13da1572cd7861da6f9fa2d47)
- License: Harbour's GPL with the Harbour exception, see [`LICENSE.txt`](LICENSE.txt) (copied from the same commit)

This repository adds build/run plumbing and a body-only bootstrap boundary. No application handler has been migrated.

## What the application does

`eshop.prg` (about 400 lines) runs an HTTPS server on port `8002`, built with Harbour's `hbhttpd` library. Pages are rendered from the templates in `app/tpl/`.

| Route | Behaviour |
|---|---|
| `/app/register` | Create an account (user, name, password twice) |
| `/app/login`, `/app/logout` | Session login using a cookie |
| `/app/main` | Landing page after login |
| `/app/shopping` | Book catalogue; `?add=<code>` adds one copy to the cart |
| `/app/cart` | Cart lines with running total; `?del=<code>` removes a line |
| `/app/account`, `/app/account/edit` | View and change name or password |
| `/hello`, `/info`, `/files/*` | Health check, server info, static CSS |

State lives in xBase tables (DBF files with CDX indexes, created at startup):

| Table | Fields | Seed data |
|---|---|---|
| `users.dbf` | `USER`, `PASSWORD`, `NAME` | Empty |
| `items.dbf` | `CODE`, `TITLE`, `PRICE` | 29 books (`0001` Linux in a Nutshell, 26.67, …) |
| `carts.dbf` | `USER`, `CODE`, `AMOUNT`, `TOTAL` | Empty |

### Legacy quirks worth knowing

These are faithful to upstream and good material for a migration:

- **There is no default user.** The code creates a `demo`/`demo` user only when `users.dbf` is missing, but upstream ships an empty `users.dbf`. Register an account first.
- **HTTPS only.** The server loads `private.key` and `certificate.crt`; the entrypoint generates a self-signed pair. The header comment names different files (`privatekey.pem`, `certificate.pem`), which is an upstream inconsistency.
- **Passwords are stored in plain text** in `users.dbf`, padded to 16 characters.
- **The item index is rebuilt on every start**, because the startup check looks for `item.cdx` instead of `items.cdx`.
- The server traces requests to stdout with terminal escape codes.

## Run it

The only requirement is Docker. Harbour isn't packaged by current Debian or Ubuntu releases, so the image builds it from source at a pinned commit. The first build takes a few minutes.

```sh
docker build -t harbour-eshop .
docker run --rm -p 8002:8002 harbour-eshop
```

Then open <https://localhost:8002> and accept the self-signed certificate.

### Try it from the command line

```sh
B=https://localhost:8002; J=$(mktemp)

curl -sk -c $J -b $J -o /dev/null -d 'user=alice&name=Alice&password1=secret&password2=secret&register=1' $B/app/register
curl -sk -c $J -b $J -o /dev/null -d 'user=alice&password=secret' $B/app/login
curl -sk -c $J -b $J -o /dev/null "$B/app/shopping?add=0001"
curl -sk -c $J -b $J -o /dev/null "$B/app/shopping?add=0001"
curl -sk -c $J -b $J $B/app/cart | sed 's/<[^>]*>/ /g' | tr -s ' \n' ' '
# … Your cart is worth: 53.34 … 0001 Linux in a Nutshell 2 53.34 …
```

To look at the data files afterwards, mount a copy of `app/` (for example `-v "$PWD/data:/app"` after copying `app/` into `data/`) or use `docker cp`.

### Build without Docker

With Harbour installed from source (`make install` in `harbour/core`, with `libssl-dev` available so `hbssl` is built):

```sh
cd app
hbmk2 eshop.prg ../boundary/selector.prg ../boundary/process.prg ../boundary/ownership.prg
# uses hbmk.hbm: hbhttpd + hbssl, -w3 -es2
openssl req -x509 -newkey rsa:2048 -nodes -days 730 -subj "/CN=localhost" \
  -keyout private.key -out certificate.crt
./eshop                # ./eshop //stop stops it from another shell
```

## Repository layout

```
app/                     upstream application with one mount-table hook
  eshop.prg              the application
  hbmk.hbm               build options (hbhttpd, hbssl)
  users.dbf items.dbf carts.dbf
  tpl/                   HTML templates
  files/main.css
Dockerfile               builds Harbour, compiles eshop, packages the runtime
docker-entrypoint.sh     creates a self-signed certificate, starts eshop
.github/workflows/       CI: compiles eshop and checks that the server answers
boundary/                native selector, bounded adapter, empty ownership registry
slices/                  library-only Rust contract; examples are test fixtures
tests/                   native and temporary-container acceptance checks
```

## CI

[`build.yml`](.github/workflows/build.yml) retains the legacy Docker build and HTTPS smoke check. It also checks Rust formatting, Clippy, unit tests, optional packaging, native selection/failure checks, and focused HTTP compatibility/rollback checks. All explicit selection uses a test-only ownership record and Rust example, never production ownership.

## Subprocess Bootstrap

The production registry in `boundary/ownership.prg` is empty. The default image remains legacy-only. Neither adding ownership nor packaging a binary activates it: only an explicit `ESHOP_ENABLED_SLICE=hello` at startup can select registered ownership. Unknown IDs, duplicate registrations, and missing or non-executable packages fail startup. Setting `hello` in this bootstrap therefore fails, intentionally.

Build the optional image without enabling anything:

```sh
docker build --target slices -t harbour-eshop:slices .
docker run --rm -p 8002:8002 harbour-eshop:slices
```

A future qualifying `/hello` implementation needs only `slices/src/bin/hello.rs` and one ownership record `{ "hello", "/hello", { "GET", "POST" } }`. Cargo discovers the executable; packaging places it at `/opt/eshop-slices/hello`. Keep its domain function separate from stdin/stdout integration. Do not add other slice types to this bootstrap.

The contract is exactly `ESHOP-BODY/1\n` on stdin followed by EOF. A helper validates it using `read_invocation`, produces only body bytes on stdout using `write_body`, then exits zero. It must finish within 1,000 milliseconds, emit at most 65,536 body bytes, and emit at most 65,536 diagnostic bytes on stderr. Empty bodies are valid. The native adapter buffers output before writing; spawn errors, deadline expiry, nonzero exit, signal termination, or excess output invoke the original stateless callback once. Diagnostics are drained but never sent to clients. Pipes close and the direct child is terminated/reaped on failure. Helpers must not spawn descendants or access `/app`; this is a contract, **not filesystem or process security isolation**.

Only exact `/hello` GET and POST requests are candidates; query strings do not change selection. Request bodies, cookies, sessions, headers, and paths never cross stdin. Harbour still handles HTTPS, method rejection, status, headers, content type, and connections. `/hello/`, adjacent paths, all other routes, and static files remain unchanged. This stateless fallback is not a replay policy for mutable routes.

To verify locally with Docker, curl, `timeout`, and standard shell tools:

```sh
docker build -t eshop-legacy-check .
docker build --target slices -t eshop-slices-check .
docker build --target boundary-test -t eshop-boundary-test .
docker build --target boundary-coverage -t eshop-boundary-coverage .
timeout 240 sh tests/check-boundary.sh
cd slices
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

Tests compare exact HTTP bodies and headers (except date/server), default/disabled/selected/cleared selection, GET/POST/query/trailing-slash/unsupported methods, root redirect, login, CSS, and unknown paths. Native and HTTP checks exercise partial-output failure after a nonzero exit or SIGKILL/SIGTERM/SIGSEGV. Native checks also cover hanging helpers, stdout/stderr bounds, absent executables, descriptor counts, and unreaped children. The coverage target measures generated C function entry coverage and the process-status wrapper, not Harbour p-code branch coverage; it is not a claim of full source coverage.

Future activation requires packaging the stateless implementation, registering ownership, passing these checks, then explicitly setting its ID and restarting using the existing deployment mechanism. Rollback clears the setting and restarts, or restores the legacy image. No data conversion is involved. A Harbour restart may invalidate process-local sessions.

Deferred: production `/hello` domain code, public gateway, orchestration, session sharing, DBF/persistence access, rendering, mutable routes, generalized HTTP forwarding, workers, telemetry infrastructure, and stricter isolation.
