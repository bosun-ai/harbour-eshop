#!/usr/bin/env python3
"""Black-box boundary checks. Fixtures never share mutable Harbour datasets."""
import argparse
import contextlib
import http.client
import json
import os
import pathlib
import re
import socket
import ssl
import subprocess
import tempfile
import threading
import time
import uuid

ROOT = pathlib.Path(__file__).resolve().parents[1]


def command(*args):
    return subprocess.check_output(args, text=True).strip()


def certificates(directory):
    for name, san in [("public", "DNS:localhost,IP:127.0.0.1"),
                      ("legacy", "DNS:legacy,DNS:localhost,IP:127.0.0.1")]:
        subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048",
                        "-nodes", "-days", "2", "-subj", f"/CN={name}",
                        "-addext", f"subjectAltName={san}",
                        "-addext", "basicConstraints=critical,CA:FALSE",
                        "-keyout", str(directory / f"{name}.key"),
                        "-out", str(directory / f"{name}.crt")],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        # Local throwaway keys must be readable by the non-root gateway image.
        (directory / f"{name}.key").chmod(0o644)


class Client:
    def __init__(self, port, ca):
        self.port = port
        self.context = ssl.create_default_context(cafile=str(ca))
        self.cookies = {}

    def request(self, path, method="GET", body=None, headers=None):
        connection = http.client.HTTPSConnection("localhost", self.port,
                                                  context=self.context, timeout=8)
        fields = {"Host": "shop.example:8002", **(headers or {})}
        if self.cookies:
            fields["Cookie"] = "; ".join(f"{key}={value}" for key, value in self.cookies.items())
        if body is not None:
            fields["Content-Type"] = "application/x-www-form-urlencoded"
        connection.request(method, path, body, fields)
        result = connection.getresponse()
        response_headers = result.getheaders()
        data = result.read()
        connection.close()
        for key, value in response_headers:
            if key.lower() == "set-cookie":
                cookie = value.split(";", 1)[0]
                name, token = cookie.split("=", 1)
                if "max-age=0" in value.lower():
                    self.cookies.pop(name, None)
                else:
                    self.cookies[name] = token
        stable = [(key.lower(), re.sub(r"SESSID=[^;]*", "SESSID=<token>", value))
                  for key, value in response_headers
                  if key.lower() in {"location", "content-type", "set-cookie"}]
        return result.status, stable, data


def wait(check):
    for _ in range(60):
        try:
            if check():
                return
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(0.25)
    raise AssertionError("fixture did not become ready")


def journey(client):
    results = []
    for path, method, body, status in [
        ("/", "GET", None, 303), ("/hello", "GET", None, 200),
        ("/missing", "GET", None, 404), ("/hello", "HEAD", None, 501),
        ("/hello", "OPTIONS", None, 501), ("/files/main.css", "GET", None, 200),
        ("/app/cart", "GET", None, 303),
        ("/app/register", "POST", "user=alice&name=Alice&password1=secret&password2=bad", 303),
        ("/app/register?err=2", "GET", None, 200),
        ("/app/register", "POST", "user=alice&name=Alice&password1=secret&password2=secret", 303),
        ("/app/main", "GET", None, 200),
        ("/app/shopping?_pos=10", "GET", None, 200),
        ("/app/shopping?add=0001", "GET", None, 303),
        ("/app/shopping?add=0001", "GET", None, 303),
        ("/app/cart", "GET", None, 200),
        ("/app/logout", "GET", None, 200),
        ("/app/login", "POST", "user=alice&password=secret", 303),
        ("/app/account", "GET", None, 200),
        ("/hello?raw=%2f%2B&raw=two+words", "GET", None, 200),
        ("/%68ello", "GET", None, 404),
    ]:
        result = client.request(path, method, body)
        assert result[0] == status, (path, result)
        if path == "/app/cart" and status == 200:
            assert b"53.34" in result[2]
        if path == "/app/register?err=2":
            assert b"Alice" in result[2] and b"alice" in result[2]
        results.append(result)
    return results


class Fixtures:
    def __init__(self, directory, legacy_image, gateway_image):
        self.directory = directory
        self.legacy_image = legacy_image
        self.gateway_image = gateway_image
        self.prefix = "eshop-check-" + uuid.uuid4().hex[:10]
        self.containers = []
        self.volumes = []
        self.network = self.prefix

    def __enter__(self):
        # Diagnostic loopback publications are test-only. Deployment uses an
        # internal upstream network and publishes only the gateway.
        command("docker", "network", "create", self.network)
        self.tls_volume = self.prefix + "-tls"
        command("docker", "volume", "create", self.tls_volume)
        self.volumes.append(self.tls_volume)
        helper = self.prefix + "-tls-copy"
        command("docker", "create", "--name", helper, "-v", f"{self.tls_volume}:/tls", self.legacy_image)
        try:
            for filename in ["public.crt", "public.key", "legacy.crt"]:
                command("docker", "cp", str(self.directory / filename), helper + ":/tls/" + filename)
        finally:
            command("docker", "rm", helper)
        return self

    def __exit__(self, *unused):
        if unused and unused[0] is not None:
            for container in self.containers:
                subprocess.run(["docker", "logs", "--tail", "12", container])
        for container in reversed(self.containers):
            subprocess.run(["docker", "rm", "-f", container], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for volume in self.volumes:
            command("docker", "volume", "rm", volume)
        command("docker", "network", "rm", self.network)

    def legacy(self, suffix, alias=None):
        name = self.prefix + suffix
        volume = name + "-data"
        command("docker", "volume", "create", volume)
        self.volumes.append(volume)
        self.containers.append(name)
        command("docker", "create", "--name", name, "--network", self.network,
                "--network-alias", alias or suffix, "-p", "127.0.0.1::8002",
                "-v", f"{volume}:/app", self.legacy_image)
        command("docker", "cp", str(self.directory / "legacy.crt"), name + ":/app/certificate.crt")
        command("docker", "cp", str(self.directory / "legacy.key"), name + ":/app/private.key")
        command("docker", "start", name)
        client = Client(self.port(name, 8002), self.directory / "legacy.crt")
        wait(lambda: client.request("/hello")[0] == 200)
        return name, client

    @staticmethod
    def port(name, port):
        return int(command("docker", "port", name, str(port)).rsplit(":", 1)[1])

    def gateway(self, suffix="-gateway", **overrides):
        name = self.prefix + suffix
        self.containers.append(name)
        settings = {"GATEWAY_TLS_CERT": "/tls/public.crt", "GATEWAY_TLS_KEY": "/tls/public.key",
                    "GATEWAY_LEGACY_CA": "/tls/legacy.crt", "GATEWAY_LEGACY_URL": "https://legacy:8002",
                    "GATEWAY_ADMIN_BIND": "0.0.0.0:8003", "GATEWAY_CONNECT_SECONDS": "1",
                    "GATEWAY_HEADER_SECONDS": "2", "GATEWAY_IDLE_SECONDS": "2", "GATEWAY_DRAIN_SECONDS": "2",
                    **overrides}
        args = ["docker", "run", "-d", "--name", name, "--network", self.network,
                "-p", "127.0.0.1::8002", "-p", "127.0.0.1::8003"]
        for key, value in settings.items():
            args += ["-e", f"{key}={value}"]
        args += ["-v", f"{self.tls_volume}:/tls:ro"]
        command(*args, self.gateway_image)
        return name

    def health(self, name, path="/ready"):
        connection = http.client.HTTPConnection("localhost", self.port(name, 8003), timeout=8)
        connection.request("GET", path)
        result = connection.getresponse()
        result.read()
        connection.close()
        return result.status


def raw_request(client, payload):
    with socket.create_connection(("localhost", client.port), timeout=6) as tcp:
        with client.context.wrap_socket(tcp, server_hostname="localhost") as stream:
            stream.sendall(payload)
            result = b""
            while True:
                data = stream.recv(65536)
                if not data:
                    break
                result += data
            return result


def transport_checks(fixtures):
    """A tiny raw TLS peer observes wire semantics and deliberate failures."""
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(fixtures.directory / "legacy.crt", fixtures.directory / "legacy.key")
    listener = socket.socket()
    listener.bind(("0.0.0.0", 0))
    listener.listen()
    listener.settimeout(0.2)
    observed = []
    stopping = threading.Event()

    def serve():
        while not stopping.is_set():
            try:
                tcp, _ = listener.accept()
            except socket.timeout:
                continue
            try:
                with context.wrap_socket(tcp, server_side=True) as stream:
                    stream.settimeout(4)
                    data = b""
                    while b"\r\n\r\n" not in data:
                        chunk = stream.recv(8192)
                        if not chunk:
                            raise ConnectionError("client closed before headers")
                        data += chunk
                    head, body = data.split(b"\r\n\r\n", 1)
                    length = re.search(br"(?im)^content-length: (\d+)", head)
                    while length and len(body) < int(length[1]):
                        chunk = stream.recv(8192)
                        if not chunk:
                            raise ConnectionError("client closed before body")
                        body += chunk
                    observed.append((head, body))
                    if b"/drop?" in head:
                        continue
                    if b"/stall " in head:
                        time.sleep(3)
                        continue
                    if b"/partial " in head:
                        stream.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\nshort")
                        time.sleep(3)
                        continue
                    stream.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close, X-Private\r\nX-Private: secret\r\nSet-Cookie: a=1; path=/\r\nSet-Cookie: b=2; Max-Age=0\r\nLocation: https://shop.example/elsewhere\r\nContent-Type: text/plain\r\nContent-Encoding: custom\r\n\r\nwire")
            except (OSError, ssl.SSLError):
                tcp.close()

    worker = threading.Thread(target=serve, daemon=True)
    worker.start()
    # This disposable peer uses localhost SAN verification; deployment uses
    # private Docker DNS instead, never host networking.
    env = {"GATEWAY_TLS_CERT": str(fixtures.directory / "public.crt"), "GATEWAY_TLS_KEY": str(fixtures.directory / "public.key"),
           "GATEWAY_LEGACY_CA": str(fixtures.directory / "legacy.crt"), "GATEWAY_LEGACY_URL": f"https://localhost:{listener.getsockname()[1]}",
           "GATEWAY_PUBLIC_BIND": "127.0.0.1:0", "GATEWAY_ADMIN_BIND": "127.0.0.1:0"}
    # The raw peer runs locally. Exercise the real cargo-built entrypoint here,
    # which also works when Docker uses a remote daemon.
    def free_port():
        with contextlib.closing(socket.socket()) as sock:
            sock.bind(("127.0.0.1", 0))
            return sock.getsockname()[1]
    port, admin = free_port(), free_port()
    env.update(GATEWAY_PUBLIC_BIND=f"127.0.0.1:{port}", GATEWAY_ADMIN_BIND=f"127.0.0.1:{admin}", GATEWAY_HEADER_SECONDS="1", GATEWAY_IDLE_SECONDS="1", GATEWAY_DRAIN_SECONDS="1")
    log = tempfile.TemporaryFile(mode="w+")
    process = subprocess.Popen([str(ROOT / "gateway/target/debug/eshop-gateway")], env={**os.environ, **env}, stdout=log, stderr=log)
    client = Client(port, fixtures.directory / "public.crt")
    try:
        wait(lambda: client.request("/wire")[0] == 200)
        assert client.request("/wire", method="PRIVATE-METHOD")[0] == 200
        data = raw_request(client, b"POST /raw%2fpath?q=%2B&q=two+words HTTP/1.1\r\nHost: original.example\r\nContent-Length: 3\r\nConnection: close, X-Remove\r\nX-Remove: no\r\nCookie: private=opaque\r\n\r\na=b")
        assert b"X-Private:" not in data and data.count(b"set-cookie:") == 2, data
        assert b"https://shop.example/elsewhere" in data and data.endswith(b"wire")
        head, body = observed[-1]
        assert b"/raw%2fpath?q=%2B&q=two+words" in head and b"original.example" in head and body == b"a=b"
        assert b"X-Remove" not in head and b"private=opaque" in head
        count = len(observed)
        assert client.request("/drop?add=0001")[0] == 502
        time.sleep(0.2)
        assert len(observed) == count + 1, "mutation GET was replayed"
        assert client.request("/stall")[0] == 504
        time.sleep(2.1)
        try:
            client.request("/partial")
            raise AssertionError("partial stream was accepted")
        except (http.client.IncompleteRead, OSError):
            pass
        time.sleep(2.1)
        # An aborted client must not cause a replay or kill the listener.
        with socket.create_connection(("localhost", port), timeout=4) as tcp:
            with client.context.wrap_socket(tcp, server_hostname="localhost") as stream:
                stream.sendall(b"GET /wire HTTP/1.1\r\nHost: localhost\r\n\r\n")
        wait(lambda: client.request("/wire")[0] == 200)
        log.flush()
        log.seek(0)
        logs = log.read()
        assert "private=opaque" not in logs and "two+words" not in logs and "original.example" not in logs
        assert "PRIVATE-METHOD" not in logs and "OTHER" in logs
        # Hold a TLS handshake open during shutdown of the actual local binary.
        with socket.create_connection(("localhost", port), timeout=4):
            started = time.monotonic()
            process.terminate()
            process.wait(timeout=4)
            assert time.monotonic() - started < 3
    finally:
        if process.poll() is None:
            process.terminate()
        process.wait(timeout=4)
        log.close()
        stopping.set()
        worker.join(timeout=5)
        listener.close()


def verify(args):
    with tempfile.TemporaryDirectory(prefix="eshop-tls-") as temporary:
        directory = pathlib.Path(temporary)
        directory.chmod(0o755)
        certificates(directory)
        with Fixtures(directory, args.legacy_image, args.gateway_image) as fixtures:
            direct_name, direct = fixtures.legacy("-direct")
            gateway_name = fixtures.gateway()
            wait(lambda: fixtures.health(gateway_name, "/live") == 200)
            assert fixtures.health(gateway_name) == 503, "gateway must start without Harbour"
            legacy_name, rollback = fixtures.legacy("-legacy", "legacy")
            proxied = Client(fixtures.port(gateway_name, 8002), directory / "public.crt")
            wait(lambda: fixtures.health(gateway_name) == 200)
            assert fixtures.health(gateway_name, "/live") == 200
            assert journey(direct) == journey(proxied), "direct/proxy behavior differs"
            assert proxied.request("/ready")[0] == 404, "private health shadows Harbour"
            direct_info = direct.request("/info")[2]
            proxy_info = proxied.request("/info", headers={"X-Observe": "host-check"})[2]
            assert b"shop.example:8002" in proxy_info and b"host-check" in proxy_info
            assert direct_info != proxy_info, "expected diagnostic peer/header differences"
            chunked = raw_request(proxied, b"POST /app/register HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\na=b\r\n0\r\n\r\n")
            assert chunked.startswith(b"HTTP/1.1 501"), chunked
            expect = raw_request(proxied, b"POST /app/login HTTP/1.1\r\nHost: localhost\r\nContent-Length: 26\r\nExpect: 100-continue\r\nConnection: close\r\n\r\nuser=alice&password=secret")
            # Hyper may emit a local 100; final Harbour response must be preserved.
            assert b"303" in expect, expect
            command("docker", "stop", "-t", "5", legacy_name)
            assert fixtures.health(gateway_name) == 503
            assert proxied.request("/hello")[0] == 502
            command("docker", "start", legacy_name)
            rollback.port = fixtures.port(legacy_name, 8002)
            wait(lambda: fixtures.health(gateway_name) == 200)
            assert proxied.request("/app/cart")[0] == 303, "Harbour restart retained session unexpectedly"
            proxied.request("/app/login", "POST", "user=alice&password=secret")
            assert b"53.34" in proxied.request("/app/cart")[2]
            rollback.cookies = proxied.cookies.copy()
            started = time.monotonic()
            command("docker", "stop", "-t", "5", gateway_name)
            assert time.monotonic() - started < 5
            assert rollback.request("/app/cart")[0] == 200, "gateway rollback lost running Harbour session"
            assert b"53.34" in rollback.request("/app/cart")[2]
            for suffix, overrides in [
                ("-unknown", {"GATEWAY_ENABLED_SLICES": "not-registered"}),
                ("-missing", {"GATEWAY_TLS_CERT": "/missing"}),
                ("-invalid", {"GATEWAY_LEGACY_URL": "http://legacy:8002"}),
                ("-invalid-key", {"GATEWAY_TLS_KEY": "/tls/legacy.crt"}),
                ("-invalid-timeout", {"GATEWAY_IDLE_SECONDS": "0"}),
                ("-invalid-log", {"GATEWAY_LOG_LEVEL": "verbose"}),
            ]:
                name = fixtures.gateway(suffix, **overrides)
                wait(lambda: command("docker", "inspect", "-f", "{{.State.Status}}", name) == "exited")
                assert command("docker", "inspect", "-f", "{{.State.ExitCode}}", name) == "1"
            for suffix, overrides in [
                ("-wrong-host", {"GATEWAY_LEGACY_URL": "https://wrong:8002"}),
                ("-wrong-trust", {"GATEWAY_LEGACY_CA": "/tls/public.crt"}),
            ]:
                if suffix == "-wrong-host":
                    command("docker", "network", "disconnect", fixtures.network, legacy_name)
                    command("docker", "network", "connect", "--alias", "legacy", "--alias", "wrong", fixtures.network, legacy_name)
                name = fixtures.gateway(suffix, **overrides)
                wait(lambda: fixtures.health(name, "/live") == 200)
                assert fixtures.health(name) == 503
            transport_checks(fixtures)
            logs = command("docker", "logs", gateway_name)
            for secret in ["secret", "password", "SESSID", "add=0001", "Alice"]:
                assert secret not in logs, ("gateway log leak", secret)
            assert "request_id" in logs and "gateway stopped" in logs
    if args.project:
        # Non-mutating probes only: never use the operator's runtime as a fixture.
        ids = command("docker", "compose", "-p", args.project, "-f", str(ROOT / "compose.bootstrap.yml"), "ps", "-q", "gateway", "legacy").splitlines()
        assert len(ids) == 2
        inspected = json.loads(command("docker", "inspect", *ids))
        gateway = next(item for item in inspected if item["Config"]["Image"] == args.gateway_image)
        legacy = next(item for item in inspected if item is not gateway)
        assert not legacy["HostConfig"]["PortBindings"], "legacy publicly published"
        assert "8003/tcp" not in gateway["HostConfig"]["PortBindings"], "admin publicly published"
        assert not any(mount["Destination"] == "/app" for mount in gateway["Mounts"])
        assert Client(8002, ROOT / "local/tls/public.crt").request("/hello")[2] == b"Hello!"
    print("PASS: isolated journeys, TLS, headers, no replay, failure, readiness, restart, rollback, shutdown, and logs")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", help="also inspect an already-running opt-in Compose project")
    parser.add_argument("--legacy-image", default="harbour-eshop:bootstrap")
    parser.add_argument("--gateway-image", default="eshop-gateway:bootstrap")
    verify(parser.parse_args())
