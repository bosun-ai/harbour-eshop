#!/usr/bin/env python3
"""Disposable black-box checks. Uses Docker, OpenSSL and Python's standard library only."""

import argparse
import concurrent.futures
import contextlib
import http.client
import http.server
import json
import os
from pathlib import Path
import re
import signal
import sys
import socket
import ssl
import subprocess
import tempfile
import threading
import time
import uuid


def command(*args):
    return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT).strip()


def certificate(directory, name, san):
    command("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2",
            "-subj", f"/CN={name}", "-addext", f"subjectAltName={san}",
            "-addext", "basicConstraints=critical,CA:FALSE",
            "-keyout", str(directory / f"{name}.key"),
            "-out", str(directory / f"{name}.crt"))
    (directory / f"{name}.key").chmod(0o644)  # Disposable keys; container runs unprivileged.


class Endpoint:
    def __init__(self, port, ca=None):
        self.port = port
        self.context = ssl.create_default_context(cafile=str(ca)) if ca else None
        self.cookies = {}

    def request(self, path, method="GET", body=None, headers=None):
        connection = (http.client.HTTPSConnection("localhost", self.port, context=self.context, timeout=8)
                      if self.context else http.client.HTTPConnection("localhost", self.port, timeout=8))
        fields = {"Host": "shop.example", **(headers or {})}
        if self.cookies:
            fields["Cookie"] = "; ".join(f"{key}={value}" for key, value in self.cookies.items())
        if body is not None:
            fields["Content-Type"] = "application/x-www-form-urlencoded"
        try:
            connection.request(method, path, body=body, headers=fields)
            response = connection.getresponse()
            pairs = response.getheaders()
            data = response.read()
            for key, value in pairs:
                if key.lower() == "set-cookie":
                    name, content = value.split(";", 1)[0].split("=", 1)
                    self.cookies[name] = content
            return response.status, pairs, data
        finally:
            connection.close()


def normalize(data):
    return re.sub(rb"SESSID=[A-Za-z0-9]+", b"SESSID=<session>", data)


def compare(first, second):
    assert first[0] == second[0], (first[0], second[0])
    selected = {"location", "content-type", "set-cookie", "last-modified", "etag", "cache-control"}
    def headers(response):
        return [(key.lower(), normalize(value.encode())) for key, value in response[1]
                if key.lower() in selected]
    assert headers(first) == headers(second), (headers(first), headers(second))
    assert normalize(first[2]) == normalize(second[2]), "body mismatch"


def wait(check, seconds=30):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            if check():
                return
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(0.1)
    raise AssertionError("startup/probe deadline")


class Runtime:
    def __init__(self, directory, legacy_image, gateway_image):
        self.directory = directory
        self.legacy_image = legacy_image
        self.gateway_image = gateway_image
        self.prefix = "eshop-verify-" + uuid.uuid4().hex[:10]
        self.containers = []
        self.processes = []
        self.volumes = []
        self.network = self.prefix

    def __enter__(self):
        command("docker", "network", "create", self.network)
        self.bridge = json.loads(command("docker", "network", "inspect", self.network))[0]["IPAM"]["Config"][0]["Gateway"]
        return self

    def __exit__(self, error_type, *_):
        if error_type:
            for name in self.containers:
                print(f"Failure diagnostics: {name}")
                subprocess.run(["docker", "logs", "--tail", "20", name], check=False)
        for process in self.processes:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        for name in reversed(self.containers):
            subprocess.run(["docker", "rm", "-f", name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
        for volume in self.volumes:
            subprocess.run(["docker", "volume", "rm", volume], stdout=subprocess.DEVNULL, check=False)
        subprocess.run(["docker", "network", "rm", self.network], stdout=subprocess.DEVNULL, check=False)

    def port(self, name, port):
        return int(command("docker", "port", name, f"{port}/tcp").split(":")[-1])

    def legacy(self):
        name = self.prefix + "-legacy"
        self.containers.append(name)
        command("docker", "create", "--name", name, "--network", self.network,
                "--network-alias", "legacy", "-p", "127.0.0.1::8002", self.legacy_image)
        command("docker", "cp", str(self.directory / "upstream.crt"), name + ":/app/certificate.crt")
        command("docker", "cp", str(self.directory / "upstream.key"), name + ":/app/private.key")
        command("docker", "start", name)
        return name, Endpoint(self.port(name, 8002), self.directory / "upstream.crt")

    def config(self, name, upstream="https://legacy:8002/", changes=None, local=False):
        text = (Path(__file__).resolve().parents[1] / "config.example.toml").read_text()
        text = text.replace("https://legacy:8002/", upstream)
        text = text.replace("/run/gateway", str(self.directory) if local else "/run/gateway")
        text = text.replace("upstream_response_ms = 150000", "upstream_response_ms = 1200")
        text = text.replace("body_idle_ms = 130000", "body_idle_ms = 700")
        text = text.replace("client_header_ms = 10000", "client_header_ms = 700")
        text = text.replace("readiness_ms = 3000", "readiness_ms = 1000")
        for old, new in (changes or {}).items():
            text = text.replace(old, new)
        path = self.directory / (name + ".toml")
        path.write_text(text)
        return path

    def gateway(self, suffix="gateway", upstream="https://legacy:8002/", changes=None):
        name = self.prefix + "-" + suffix
        config = self.config(suffix, upstream, changes)
        volume = name + "-inputs"
        self.volumes.append(volume)
        command("docker", "volume", "create", volume)
        helper = name + "-stage"
        self.containers.append(helper)
        command("docker", "create", "--name", helper, "--entrypoint", "true",
                "-v", f"{volume}:/inputs", self.legacy_image)
        for source, destination in [(config, "config.toml"),
                                    (self.directory / "public.crt", "public.crt"),
                                    (self.directory / "public.key", "public.key"),
                                    (self.directory / "upstream.crt", "upstream.crt")]:
            command("docker", "cp", str(source), helper + ":/inputs/" + destination)
        command("docker", "rm", helper)
        self.containers.remove(helper)
        self.containers.append(name)
        command("docker", "run", "-d", "--name", name, "--network", self.network,
                "-p", "127.0.0.1::8002", "-p", "127.0.0.1::9000",
                "-e", "GATEWAY_CONFIG=/run/gateway/config.toml",
                "-v", f"{volume}:/run/gateway:ro", self.gateway_image)
        return name

    def endpoints(self, name):
        return (Endpoint(self.port(name, 8002), self.directory / "public.crt"),
                Endpoint(self.port(name, 9000)))


class TestUpstream(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    counts = {}
    lock = threading.Lock()

    def log_message(self, *_):
        pass

    def do_POST(self):
        self.do_GET()

    def do_GET(self):
        with self.lock:
            self.counts[self.path] = self.counts.get(self.path, 0) + 1
        if self.path == "/disconnect":
            self.connection.shutdown(socket.SHUT_RDWR)
            self.connection.close()
            return
        if self.path in ("/slow", "/drain"):
            time.sleep(2 if self.path == "/slow" else 0.5)
        length = int(self.headers.get("Content-Length", "0"))
        data = self.rfile.read(length)
        if self.path == "/counts":
            body = json.dumps(self.counts).encode()
        elif self.path == "/hello":
            body = b"Hello!"
        elif self.path == "/large":
            body = b"x" * (2 * 1024 * 1024)
        elif self.path == "/backpressure":
            body = b"x" * (128 * 1024 * 1024)
        else:
            body = json.dumps({"path": self.path, "method": self.command,
                               "body": data.decode(), "headers": dict(self.headers)}).encode()
        try:
            self.send_response(200)
            self.send_header("Set-Cookie", "a=1; path=/")
            self.send_header("Set-Cookie", "b=2; path=/")
            self.send_header("Location", "relative?x=1")
            self.send_header("Connection", "x-hop")
            self.send_header("x-hop", "remove")
            self.send_header("Content-Length", str(len(body) + (10 if self.path == "/truncated" else 0)))
            self.end_headers()
            if self.path == "/idle":
                self.wfile.flush()
                time.sleep(1.5)
            self.wfile.write(body)
            if self.path == "/truncated":
                self.connection.shutdown(socket.SHUT_RDWR)
                self.connection.close()
        except OSError:
            pass


def raw_request(endpoint, payload):
    with socket.create_connection(("localhost", endpoint.port), timeout=5) as connection:
        with endpoint.context.wrap_socket(connection, server_hostname="localhost") as stream:
            stream.sendall(payload)
            return stream.recv(4096)


def legacy_checks(runtime):
    legacy_name, direct = runtime.legacy()
    wait(lambda: direct.request("/hello")[2] == b"Hello!")
    gateway_name = runtime.gateway()
    proxy, health = runtime.endpoints(gateway_name)
    wait(lambda: health.request("/ready")[0] == 200)
    for path, method in [("/hello", "GET"), ("/", "GET"), ("/missing", "GET"),
                         ("/live", "GET"), ("/ready", "GET"), ("/hello", "PATCH"),
                         ("/files/main.css", "GET"), ("/app/main", "GET")]:
        compare(direct.request(path, method), proxy.request(path, method))
    css = direct.request("/files/main.css")
    modified = next(value for key, value in css[1] if key.lower() == "last-modified")
    compare(direct.request("/files/main.css", headers={"If-Modified-Since": modified}),
            proxy.request("/files/main.css", headers={"If-Modified-Since": modified}))
    assert proxy.request("/hello")[0::2] == (200, b"Hello!")

    # Share a session against the same state owner, but execute every write exactly once.
    direct.request("/app/register", "POST", "user=fixture&name=Retained&password1=x&password2=y")
    proxy.cookies = direct.cookies.copy()
    compare(direct.request("/app/register?err=2"), proxy.request("/app/register?err=2"))
    assert b"Retained" in proxy.request("/app/register?err=2")[2]
    proxy.request("/app/register", "POST", "user=fixture&name=Retained&password1=&password2=")
    compare(direct.request("/app/register?err=1"), proxy.request("/app/register?err=1"))
    response = proxy.request("/app/register", "POST", "user=fixture&name=Fixture&password1=secret&password2=secret")
    assert response[0] == 303 or response[0] == 302
    direct.cookies = proxy.cookies.copy()
    proxy.request("/app/register", "POST", "user=fixture&name=Duplicate&password1=secret&password2=secret")
    compare(direct.request("/app/register?err=3"), proxy.request("/app/register?err=3"))
    for path in ["/app/main", "/app/account", "/app/account/edit", "/app/shopping", "/app/shopping?_pos=10", "/app/cart"]:
        compare(direct.request(path), proxy.request(path))
    proxy.request("/app/account/edit", "POST", "name=Updated&password1=&password2=")
    assert b"Updated" in direct.request("/app/account")[2]
    compare(direct.request("/app/account"), proxy.request("/app/account"))
    proxy.request("/app/account/edit", "POST", "name=RetainedEdit&password1=x&password2=y")
    compare(direct.request("/app/account/edit?err=2"), proxy.request("/app/account/edit?err=2"))
    assert b"RetainedEdit" in proxy.request("/app/account/edit?err=2")[2]
    proxy.request("/app/account/edit", "POST", "name=&password1=&password2=")
    compare(direct.request("/app/account/edit?err=1"), proxy.request("/app/account/edit?err=1"))
    direct.request("/app/shopping?add=0001")
    proxy.request("/app/shopping?add=0001")
    compare(direct.request("/app/cart"), proxy.request("/app/cart"))
    assert b"53.34" in proxy.request("/app/cart")[2]
    proxy.request("/app/cart?del=0001")
    compare(direct.request("/app/cart"), proxy.request("/app/cart"))

    info = proxy.request("/info", headers={"X-Forwarded-For": "spoof-marker", "Forwarded": "for=spoof-marker", "X-Request-ID": "spoof-marker"})[2]
    assert b"spoof-marker" not in info
    assert b"HTTP_X_FORWARDED_FOR" in info and b"HTTP_X_REQUEST_ID" in info
    assert b"HTTP_HOST" in info and b"shop.example" in info
    assert b"REMOTE_ADDR" in info
    gateway_ip = json.loads(command("docker", "inspect", gateway_name))[0]["NetworkSettings"]["Networks"][runtime.network]["IPAddress"]
    assert gateway_ip.encode() in info, "diagnostic peer should be gateway, not public client"
    assert b"HTTP_X_FORWARDED_PROTO" in info and b"https" in info
    assert b"HTTP_X_REQUEST_ID" not in direct.request("/info")[2]
    mounts = json.loads(command("docker", "inspect", gateway_name))[0]["Mounts"]
    assert len(mounts) == 1 and all(not mount["RW"] for mount in mounts)
    assert all("/app" not in mount["Destination"] and "upstream.key" not in mount["Source"] for mount in mounts)

    command("docker", "restart", gateway_name)
    proxy.port = runtime.port(gateway_name, 8002)
    health.port = runtime.port(gateway_name, 9000)
    wait(lambda: health.request("/ready")[0] == 200)
    assert b"Updated" in proxy.request("/app/account")[2], "gateway restart lost legacy session"
    command("docker", "stop", legacy_name)
    assert health.request("/live")[0] == 200
    assert health.request("/ready")[0] == 503
    assert proxy.request("/hello")[0] == 502
    command("docker", "start", legacy_name)
    direct.port = runtime.port(legacy_name, 8002)
    wait(lambda: health.request("/ready")[0] == 200)
    assert proxy.request("/app/account")[0] in (302, 303), "legacy restart should lose session"
    proxy.request("/app/login", "POST", "user=fixture&password=secret")
    assert b"Updated" in proxy.request("/app/account")[2], "account did not persist"
    direct.cookies = proxy.cookies.copy()
    command("docker", "stop", gateway_name)
    assert direct.request("/hello")[2] == b"Hello!"
    assert b"Updated" in direct.request("/app/account")[2], "rollback must retain same owner/session"
    assert direct.request("/app/shopping")[0] == 200
    assert direct.request("/app/cart")[0] == 200
    direct.request("/app/logout")
    assert direct.request("/app/main")[0] in (302, 303)
    direct.request("/app/login", "POST", "user=fixture&password=wrong")
    assert b"Invalid user name or password" in direct.request("/app/login?err")[2]
    print("PASS legacy compatibility, framing-sensitive forms, diagnostics, state/session lifecycle and rollback")


def adapter_checks(runtime, binary):
    def test_server(suffix, identity):
        name = runtime.prefix + suffix
        runtime.containers.append(name)
        command("docker", "create", "--name", name, "--network", runtime.network,
                "--network-alias", suffix, "-p", "127.0.0.1::8002", "python:3-slim",
                "python", "/verify.py", "--serve")
        command("docker", "cp", str(Path(__file__).resolve()), name + ":/verify.py")
        command("docker", "cp", str(runtime.directory / f"{identity}.crt"), name + ":/certificate.crt")
        command("docker", "cp", str(runtime.directory / f"{identity}.key"), name + ":/private.key")
        command("docker", "start", name)
        endpoint = Endpoint(runtime.port(name, 8002), runtime.directory / f"{identity}.crt")
        wait(lambda: endpoint.request("/hello")[2] == b"Hello!")
        return endpoint

    probe = test_server("adapter-upstream", "upstream")
    def counts():
        return json.loads(probe.request("/counts")[2])
    try:
        upstream = "https://adapter-upstream:8002/"
        name = runtime.gateway("adapter", upstream, { 'log_level = "info"': 'log_level = "trace"' })
        endpoint, health = runtime.endpoints(name)
        wait(lambda: health.request("/ready")[0] == 200)
        response = endpoint.request("/echo%2Fraw?a=1&a=2", "POST", "secret-body", {
            "Connection": "x-hop", "x-hop": "remove", "X-Forwarded-For": "spoof", "X-Request-ID": "spoof"})
        data = json.loads(response[2])
        data["headers"] = {key.lower(): value for key, value in data["headers"].items()}
        assert data["path"] == "/echo%2Fraw?a=1&a=2" and data["body"] == "secret-body"
        assert data["method"] == "POST" and data["headers"]["host"] == "shop.example"
        assert data["headers"]["content-length"] == "11"
        assert "x-hop" not in {key.lower() for key in data["headers"]}
        assert "transfer-encoding" not in data["headers"]
        assert "spoof" not in data["headers"].values()
        assert [value for key, value in response[1] if key.lower() == "set-cookie"] == ["a=1; path=/", "b=2; path=/"]
        assert any(key.lower() == "location" and value == "relative?x=1" for key, value in response[1])
        assert not any(key.lower() == "x-hop" for key, _ in response[1])
        assert len(endpoint.request("/large")[2]) == 2 * 1024 * 1024
        assert endpoint.request("/slow")[0] == 504
        assert endpoint.request("/disconnect")[0] == 502
        assert counts()["/disconnect"] == 1 and counts()["/slow"] == 1
        for path in ["/idle", "/truncated"]:
            try:
                endpoint.request(path)
                raise AssertionError("incomplete stream should abort")
            except (http.client.IncompleteRead, ConnectionError, ssl.SSLError):
                pass
        raw = raw_request(endpoint, b"POST /chunked HTTP/1.1\r\nHost: shop.example\r\nTransfer-Encoding: chunked\r\n\r\n4\r\ntest\r\n0\r\n\r\n")
        assert b" 411 " in raw, raw
        raw = raw_request(endpoint, b"POST /ambiguous HTTP/1.1\r\nHost: shop.example\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n")
        assert b" 400 " in raw or b" 411 " in raw, raw
        assert "/chunked" not in counts() and "/ambiguous" not in counts()
        with socket.create_connection(("localhost", endpoint.port), timeout=5) as connection:
            with endpoint.context.wrap_socket(connection, server_hostname="localhost") as stream:
                stream.sendall(b"GET /hello HTTP/1.1\r\nHost:")
                time.sleep(1)
                assert stream.recv(4096) == b"", "header timeout did not close connection"
        with socket.create_connection(("localhost", endpoint.port), timeout=5) as connection:
            with endpoint.context.wrap_socket(connection, server_hostname="localhost") as stream:
                stream.sendall(b"POST /slow-upload HTTP/1.1\r\nHost: shop.example\r\nContent-Length: 10\r\n\r\na")
                time.sleep(1)
                result = stream.recv(4096)
                assert b" 502 " in result or result == b"", "stalled upload was not terminated"
        assert counts().get("/slow-upload", 0) <= 1

        # A real native local-run entrypoint, including invalid startup and graceful drain.
        if binary:
            with contextlib.closing(socket.socket()) as public_socket, contextlib.closing(socket.socket()) as management_socket:
                public_socket.bind(("127.0.0.1", 0))
                management_socket.bind(("127.0.0.1", 0))
                public_port, management_port = public_socket.getsockname()[1], management_socket.getsockname()[1]
            changes = {"0.0.0.0:8002": f"127.0.0.1:{public_port}", "0.0.0.0:9000": f"127.0.0.1:{management_port}"}
            config = runtime.config("native", f"https://localhost:{probe.port}/", changes, local=True)
            log = runtime.directory / "native.log"
            with log.open("w") as output:
                process = subprocess.Popen([str(binary)], env={**os.environ, "GATEWAY_CONFIG": str(config)}, stdout=output, stderr=output)
                runtime.processes.append(process)
                native = Endpoint(public_port, runtime.directory / "public.crt")
                wait(lambda: native.request("/hello")[2] == b"Hello!")
                with concurrent.futures.ThreadPoolExecutor() as pool:
                    before = counts().get("/drain", 0)
                    request = pool.submit(native.request, "/drain")
                    wait(lambda: counts().get("/drain", 0) > before)
                    process.send_signal(signal.SIGTERM)
                    assert request.result()[0] == 200
                assert process.wait(timeout=5) == 0
            assert "stopped" in log.read_text()
            # Inspect the native server's TCP state without reading buffered output:
            # recv() could unblock the writer and hide the deadline regression.
            def established(client_port):
                for table in ["/proc/net/tcp", "/proc/net/tcp6"]:
                    for row in Path(table).read_text().splitlines()[1:]:
                        fields = row.split()
                        if (int(fields[1].split(":")[1], 16) == public_port
                                and int(fields[2].split(":")[1], 16) == client_port
                                and fields[3] == "01"):
                            return True
                return False

            for idle_ms, draining in [(200, False), (1000, False), (1000, True)]:
                deadline_config = runtime.config(f"backpressure-{idle_ms}-{draining}",
                    f"https://localhost:{probe.port}/", {**changes,
                        "total_request_ms = 180000": "total_request_ms = 500",
                        "client_header_ms = 700": "client_header_ms = 2000",
                        "upstream_response_ms = 1200": "upstream_response_ms = 400",
                        "body_idle_ms = 700": f"body_idle_ms = {idle_ms}"}, local=True)
                with log.open("w") as output:
                    process = subprocess.Popen([str(binary)], env={**os.environ, "GATEWAY_CONFIG": str(deadline_config)}, stdout=output, stderr=output)
                    runtime.processes.append(process)
                    wait(lambda: native.request("/hello")[2] == b"Hello!")
                    # Completed responses must not leave a total timer on idle keepalive.
                    connection = http.client.HTTPSConnection("localhost", public_port, context=native.context, timeout=3)
                    try:
                        connection.request("GET", "/hello")
                        assert connection.getresponse().read() == b"Hello!"
                        keepalive_port = connection.sock.getsockname()[1]
                        time.sleep(0.8)
                        assert established(keepalive_port), "total timer closed idle keepalive"
                        connection.request("GET", "/hello")
                        assert connection.getresponse().read() == b"Hello!"
                        connection.request("HEAD", "/hello")
                        assert connection.getresponse().read() == b""
                        time.sleep(0.6)
                        assert established(keepalive_port), "HEAD left an active total timer"
                    finally:
                        connection.close()
                    with socket.create_connection(("localhost", public_port), timeout=3) as connection:
                        connection.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
                        with native.context.wrap_socket(connection, server_hostname="localhost") as stream:
                            client_port = stream.getsockname()[1]
                            before = counts().get("/backpressure", 0)
                            stream.sendall(b"GET /backpressure HTTP/1.1\r\nHost: shop.example\r\n\r\n")
                            wait(lambda: counts().get("/backpressure", 0) > before, seconds=1)
                            assert established(client_port), "probe never established a request"
                            started = time.monotonic()
                            if draining:
                                process.terminate()
                            wait(lambda: not established(client_port), seconds=1.5)
                            assert time.monotonic() - started < 1.5, "non-reading client bypassed deadline"
                    if not draining:
                        process.terminate()
                    assert process.wait(timeout=3) == 0
                assert '"error_class":"request_deadline"' in log.read_text()
            abort_config = runtime.config("abort-drain", f"https://localhost:{probe.port}/",
                                          {**changes, "shutdown_ms = 30000": "shutdown_ms = 100"}, local=True)
            with log.open("w") as output:
                process = subprocess.Popen([str(binary)], env={**os.environ, "GATEWAY_CONFIG": str(abort_config)}, stdout=output, stderr=output)
                runtime.processes.append(process)
                wait(lambda: native.request("/hello")[2] == b"Hello!")
                with concurrent.futures.ThreadPoolExecutor() as pool:
                    before = counts().get("/slow", 0)
                    request = pool.submit(native.request, "/slow")
                    wait(lambda: counts().get("/slow", 0) > before)
                    process.terminate()
                    assert process.wait(timeout=5) == 0
                    try:
                        request.result()
                        raise AssertionError("forced drain should abort work")
                    except (http.client.HTTPException, OSError):
                        pass
            assert "shutdown_deadline" in log.read_text()
            for suffix, invalid_changes in [
                ("activation", {"enabled_families = []": 'enabled_families = ["account"]'}),
                ("missing-key", {"public.key": "missing.key"}),
                ("mismatched-key", {"public.key": "upstream.key"}),
                ("unknown-config", {"log_level =": "unknown_field = true\nlog_level ="}),
            ]:
                invalid = runtime.config(suffix, f"https://localhost:{probe.port}/", {**changes, **invalid_changes}, local=True)
                result = subprocess.run([str(binary)], env={**os.environ, "GATEWAY_CONFIG": str(invalid)}, capture_output=True, text=True, timeout=5)
                assert result.returncode != 0 and "failure:" in result.stderr
                assert "secret" not in result.stderr and str(runtime.directory) not in result.stderr

        for suffix, upstream_url, changes in [
            ("wrong-ca", upstream, {"upstream_ca = \"/run/gateway/upstream.crt\"": "upstream_ca = \"/run/gateway/public.crt\""}),
            ("wrong-host", "https://wrong-host-upstream:8002/", None),
        ]:
            if suffix == "wrong-host":
                # Separate server certificate deliberately lacks the bridge IP SAN.
                test_server("wrong-host-upstream", "hostname")
                # Gateway mount is intentionally fixed; replace trust input only while creating this instance.
                original = (runtime.directory / "upstream.crt").read_bytes()
                (runtime.directory / "upstream.crt").write_bytes((runtime.directory / "hostname.crt").read_bytes())
            bad_name = runtime.gateway(suffix, upstream_url, changes)
            bad, bad_health = runtime.endpoints(bad_name)
            wait(lambda: bad_health.request("/live")[0] == 200)
            assert bad.request("/hello")[0] == 502
            assert bad_health.request("/ready")[0] == 503
            if suffix == "wrong-host":
                (runtime.directory / "upstream.crt").write_bytes(original)
        logs = command("docker", "logs", name)
        assert all(secret not in logs for secret in ["secret-body", "shop.example", "a=1&a=2", "spoof"])
        for line in logs.splitlines():
            record = json.loads(line)
            assert not any(key in record.get("fields", {}) for key in ["body", "headers", "query", "uri"])
        print("PASS adapter framing/streaming, retries, TLS failures, logs, native activation and drain")
    finally:
        pass  # Runtime owns and removes the disposable upstream containers.


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--legacy-image", required=True)
    parser.add_argument("--gateway-image", required=True)
    parser.add_argument("--gateway-binary", type=Path,
                        default=Path(__file__).resolve().parents[1] / "target/debug/eshop-gateway")
    args = parser.parse_args()
    binary = args.gateway_binary.resolve() if args.gateway_binary.exists() else None
    if binary is None:
        print("GAP native local-run checks skipped: build gateway/target/debug/eshop-gateway first")
    with tempfile.TemporaryDirectory(prefix="eshop-verify-") as temporary:
        directory = Path(temporary)
        directory.chmod(0o755)
        with Runtime(directory, args.legacy_image, args.gateway_image) as runtime:
            certificate(directory, "public", "DNS:localhost")
            certificate(directory, "upstream", f"DNS:legacy,DNS:adapter-upstream,DNS:localhost,IP:{runtime.bridge}")
            certificate(directory, "hostname", "DNS:localhost")
            legacy_checks(runtime)
            adapter_checks(runtime, binary)
    print("PASS cleanup: disposable containers, network and TLS inputs removed")


if __name__ == "__main__":
    if sys.argv[1:] == ["--serve"]:
        server = http.server.ThreadingHTTPServer(("0.0.0.0", 8002), TestUpstream)
        server.daemon_threads = True
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain("/certificate.crt", "/private.key")
        server.socket = tls.wrap_socket(server.socket, server_side=True)
        server.serve_forever()
    else:
        main()
