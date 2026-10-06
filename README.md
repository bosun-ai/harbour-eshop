# Harbour eShop

[![build](https://github.com/bosun-ai/harbour-eshop/actions/workflows/build.yml/badge.svg)](https://github.com/bosun-ai/harbour-eshop/actions/workflows/build.yml)

A small legacy-style web shop written in [Harbour](https://github.com/harbour/core), the open-source successor of the Clipper/xBase 4GL. It is a sample application for [Bosun](https://bosun.ai) migration demos: real 4GL-era code with HTTP in, DBF files out, and business logic mixed in with rendering, just like most legacy code.

## Where the code comes from

Everything in [`app/`](app/) is copied **unmodified** from the official Harbour repository:

- Source: [`harbour/core` → `contrib/hbhttpd/tests/`](https://github.com/harbour/core/tree/529b0d42939610a13da1572cd7861da6f9fa2d47/contrib/hbhttpd/tests)
- Pinned commit: [`529b0d4`](https://github.com/harbour/core/commit/529b0d42939610a13da1572cd7861da6f9fa2d47)
- License: Harbour's GPL with the Harbour exception, see [`LICENSE.txt`](LICENSE.txt) (copied from the same commit)

This repository only adds the build and run plumbing: `Dockerfile`, `docker-entrypoint.sh`, and the CI workflow.

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

## Optional Rust ingress gateway

The default command above remains the emergency fallback and publishes Harbour directly. The optional `gateway/` crate introduces a separate public HTTPS listener while preserving Harbour as the private HTTPS upstream and sole application/state owner. Its bootstrap registry is empty, so `GATEWAY_ENABLED_SLICES=` proxies every path unchanged; adding a future handler never selects it.

Build it separately:

```sh
docker build -t harbour-eshop-gateway gateway
```

Use `gateway/gateway.env.example` as the required configuration contract. Deployment requires separate containers on a private network: publish only gateway port `8002`, do not publish the legacy container port, and do not share writable volumes (especially `/app`). Deliver the public certificate/key and legacy CA certificate as separate read-only files. The legacy certificate must contain the Docker alias used by `GATEWAY_LEGACY_URL` and match `GATEWAY_UPSTREAM_TLS_SERVER_NAME`; the stock entrypoint's `localhost` certificate cannot validate `https://legacy:8002`.

`scripts/test-gateway-proxy.sh` is the executable local contract. It creates isolated certificates and a Docker network, then compares direct private legacy and public all-proxy responses for `/hello`, `/`, and `/files/main.css`; it also checks registration, session cookies, repeated cart query mutations, and the cart total. Run it after both images are built:

```sh
LEGACY_IMAGE=harbour-eshop GATEWAY_IMAGE=harbour-eshop-gateway sh scripts/test-gateway-proxy.sh
```

No gateway health path is reserved in this bootstrap because unknown paths must continue to reach Harbour. Production liveness/readiness probing and certificate delivery are platform inputs that must be supplied by the deployment environment.

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
hbmk2 eshop.prg        # uses hbmk.hbm: hbhttpd + hbssl, -w3 -es2
openssl req -x509 -newkey rsa:2048 -nodes -days 730 -subj "/CN=localhost" \
  -keyout private.key -out certificate.crt
./eshop                # ./eshop //stop stops it from another shell
```

## Repository layout

```
app/                     unmodified upstream sources
  eshop.prg              the application
  hbmk.hbm               build options (hbhttpd, hbssl)
  users.dbf items.dbf carts.dbf
  tpl/                   HTML templates
  files/main.css
Dockerfile               builds Harbour, compiles eshop, packages the runtime
docker-entrypoint.sh     creates a self-signed certificate, starts eshop
.github/workflows/       CI: compiles eshop and checks that the server answers
gateway/                 isolated Rust public HTTPS ingress and dispatch extension point
scripts/test-gateway-proxy.sh  two-container all-proxy compatibility contract
```

## CI

[`build.yml`](.github/workflows/build.yml) builds the legacy image, builds the isolated gateway image, retains the direct `/hello` smoke, then runs the two-container all-proxy compatibility contract. The Harbour code remains upstream sample code; focused dispatcher tests live in the gateway crate.
