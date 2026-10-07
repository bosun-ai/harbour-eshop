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

### Optional Rust Ingress

The standalone [gateway bootstrap](migration/README.md) adds an opt-in HTTPS
boundary without migrating any application routes. Building or merging it does
not change the commands above. Activation requires explicit TLS/trust inputs and
an exclusive, persistent Harbour runtime; rollback must retain the current DBFs.

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
```

## CI

[`build.yml`](.github/workflows/build.yml) builds the Docker image, which compiles Harbour and `eshop.prg` with warnings treated as errors (`-w3 -es2`). It then starts the container and checks that `/hello` responds. The code is upstream sample code, so there's no separate test suite.
