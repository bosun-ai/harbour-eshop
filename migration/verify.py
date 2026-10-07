#!/usr/bin/env python3
"""Bounded, disposable entrypoint verification. Never target a live shop.

Uses only Python's standard library, openssl, Docker, and a built gateway binary.
All application mutations run on independent copies of the image runtime.
"""
import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import signal
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time


def command(*args, timeout=90):
    return subprocess.check_output(args, stderr=subprocess.STDOUT, timeout=timeout).decode().strip()


def certificate(directory, name, san):
    key, cert = directory / (name + ".key"), directory / (name + ".crt")
    command("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2",
            "-subj", "/CN=" + name, "-addext", "subjectAltName=" + san,
            "-addext", "basicConstraints=critical,CA:FALSE",
            "-keyout", str(key), "-out", str(cert))
    return key, cert


def request(port, path="/hello", method="GET", body=None, headers=None, secure=True):
    connection = (http.client.HTTPSConnection("localhost", port, timeout=5,
                  context=ssl._create_unverified_context()) if secure else
                  http.client.HTTPConnection("localhost", port, timeout=5))
    connection.request(method, path, body, headers or {})
    result = connection.getresponse()
    answer = result.status, result.getheaders(), result.read()
    connection.close()
    return answer


def wait_for(check, seconds=15):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        try:
            if check():
                return
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(0.1)
    raise AssertionError("bounded readiness wait failed")


class SyntheticUpstream:
    """Raw TLS upstream exposes exact framing and cancellation without app behavior."""
    def __init__(self, key, cert):
        self.context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        self.context.load_cert_chain(cert, key)
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(128)
        self.listener.settimeout(0.2)
        self.port = self.listener.getsockname()[1]
        self.requests = []
        self.connections = 0
        self.mode = "normal"
        self.stop = threading.Event()
        self.cancelled = threading.Event()
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        while not self.stop.is_set():
            try:
                connection, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            self.connections += 1
            threading.Thread(target=self.serve, args=(connection,), daemon=True).start()

    def serve(self, connection):
        try:
            with self.context.wrap_socket(connection, server_side=True) as stream:
                stream.settimeout(3)
                if self.mode == "preclose":
                    return
                data = b""
                while b"\r\n\r\n" not in data:
                    part = stream.recv(4096)
                    if not part:
                        return
                    data += part
                head, body = data.split(b"\r\n\r\n", 1)
                length = re.search(br"(?im)^content-length: (\d+)", head)
                length = int(length[1]) if length else 0
                while len(body) < length:
                    part = stream.recv(min(4096, length - len(body)))
                    if not part:
                        self.cancelled.set()
                        return
                    body += part
                self.requests.append((head, body))
                mode = self.mode
                if mode == "close":
                    return
                if mode in ("slow", "drain"):
                    time.sleep(1.5 if mode == "slow" else 0.2)
                if mode == "hold":
                    time.sleep(2)
                if mode in ("partial", "endless"):
                    stream.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\npart")
                    if mode == "partial":
                        time.sleep(1.5)
                    else:
                        while True:
                            stream.sendall(b"x" * 16384)
                    return
                payload = b"Hello!"
                stream.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n"
                               b"Set-Cookie: first=1; path=/\r\nSet-Cookie: second=2; path=/\r\n"
                               b"Location: https://public.example/raw%2F?b=2&a=1\r\n"
                               b"Content-Encoding: gzip\r\nConnection: close, x-private\r\n"
                               b"X-Private: hide\r\nContent-Type: text/plain\r\n\r\n" + payload)
        except (OSError, ssl.SSLError):
            self.cancelled.set()

    def close(self):
        self.stop.set()
        self.listener.close()
        self.thread.join(2)


class Gateway:
    def __init__(self, binary, directory, config):
        self.config = directory / "gateway.toml"
        self.config.write_text(config)
        self.log_path = directory / "gateway.log"
        self.log = self.log_path.open("ab")
        self.process = subprocess.Popen([binary, "--config", str(self.config)], stdout=self.log, stderr=self.log)

    def close(self):
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
        self.process.wait(timeout=5)
        self.log.close()


def configuration(key, cert, ca, origin):
    return f'''public_bind = "127.0.0.1:8002"
management_bind = "127.0.0.1:9000"
legacy_url = "{origin}"
public_cert_file = "{cert}"
public_key_file = "{key}"
upstream_ca_file = "{ca}"
trusted_proxy_cidrs = []
connect_timeout_ms = 300
header_timeout_ms = 300
body_idle_timeout_ms = 300
response_timeout_ms = 700
shutdown_drain_ms = 1000
log_level = "info"
active_families = []
'''


def raw_request(data):
    with ssl._create_unverified_context().wrap_socket(socket.create_connection(("localhost", 8002), 3), server_hostname="localhost") as stream:
        stream.settimeout(3)
        stream.sendall(data)
        result = b""
        while True:
            part = stream.recv(4096)
            if not part:
                return result
            result += part


def verify_transport(binary, directory):
    key, cert = certificate(directory, "public", "DNS:localhost")
    upstream_key, upstream_cert = certificate(directory, "upstream", "DNS:localhost")
    wrong_key, wrong_cert = certificate(directory, "wrong", "DNS:wrong.invalid")
    upstream = SyntheticUpstream(upstream_key, upstream_cert)
    base = configuration(key, cert, upstream_cert, f"https://localhost:{upstream.port}")
    # All invalid settings fail through the real CLI before sockets are bound.
    for invalid in [base.replace(str(key), str(directory / "missing.key")),
                    base.replace(str(key), str(wrong_key)),
                    base.replace('active_families = []', 'active_families = ["cart"]'),
                    base.replace('log_level = "info"', 'log_level = "password-secret"'),
                    base.replace('connect_timeout_ms = 300', 'connect_timeout_ms = 0'),
                    base.replace('127.0.0.1:9000', '0.0.0.0:9000'),
                    base.replace('127.0.0.1:8002', '127.0.0.1:8003'),
                    base.replace(f'https://localhost:{upstream.port}', 'http://user:password@localhost/path?secret'),
                    base + 'unexpected = "secret"\n']:
        path = directory / "invalid.toml"
        path.write_text(invalid)
        result = subprocess.run([binary, "--config", str(path)], capture_output=True, timeout=3)
        assert result.returncode != 0 and b"password-secret" not in result.stderr
    gateway = Gateway(binary, directory, base.replace('log_level = "info"', 'log_level = "trace"'))
    try:
        wait_for(lambda: request(9000, "/ready", secure=False)[0] == 200)
        status, headers, body = request(8002, "/raw%2fkeep?z=1&z=2&a=%2B", "POST", b"secret=bytes+%2F\x00",
                {"Host": "public.example:8002", "Cookie": "SESSID=secret-cookie", "X-Forwarded-For": "spoofed", "Forwarded": "secret", "Connection": "x-remove", "X-Remove": "secret"})
        assert status == 200 and body == b"Hello!"
        assert len([value for name, value in headers if name.lower() == "set-cookie"]) == 2
        normalized_headers = [(name.lower(), value) for name, value in headers]
        assert ("location", "https://public.example/raw%2F?b=2&a=1") in normalized_headers
        assert ("content-encoding", "gzip") in normalized_headers  # no decompression
        assert not any(name.lower() == "x-private" for name, _ in headers)
        head, sent = upstream.requests[-1]
        assert head.startswith(b"POST /raw%2fkeep?z=1&z=2&a=%2B HTTP/1.1")
        assert b"host: public.example:8002" in head.lower() and b"transfer-encoding" not in head.lower()
        assert sent == b"secret=bytes+%2F\x00" and b"SESSID=secret-cookie" in head
        assert b"spoofed" not in head and b"x-remove" not in head and b"forwarded: secret" not in head.lower()
        assert b"x-forwarded-for: 127.0.0.1" in head.lower()
        before = len(upstream.requests)
        for packet, expected in [
                (b"POST /app/login HTTP/1.1\r\nHost: public\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n", (411,)),
                (b"POST / HTTP/1.1\r\nHost: public\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nx", (400,)),
                (b"POST / HTTP/1.1\r\nHost: public\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n", (400, 411)),
                (b"GET https://public/path HTTP/1.1\r\nHost: public\r\n\r\n", (400,)),
                (b"CONNECT public:443 HTTP/1.1\r\nHost: public\r\n\r\n", (400,))]:
            answer = raw_request(packet)
            assert int(answer.split()[1]) in expected, answer
        assert len(upstream.requests) == before
        # Faults before and after submission must never trigger replay, even for GET mutations.
        for mode, expected in [("preclose", 502), ("close", 502), ("slow", 504)]:
            upstream.mode = mode
            before = upstream.connections
            assert request(8002, "/app/shopping?add=0001")[0] == expected
            time.sleep(0.1)
            assert upstream.connections == before + 1
        upstream.mode = "partial"
        try:
            request(8002)
            raise AssertionError("partial stream unexpectedly completed")
        except (http.client.IncompleteRead, OSError):
            pass
        # Cancellation closes the single upstream driver, rather than buffering a huge body.
        upstream.mode = "endless"
        upstream.cancelled.clear()
        client = http.client.HTTPSConnection("localhost", 8002, context=ssl._create_unverified_context(), timeout=3)
        client.request("GET", "/hello")
        result = client.getresponse()
        assert result.read(4) == b"part"
        result.close()
        client.close()
        assert upstream.cancelled.wait(3)
        upstream.mode = "normal"
        # Stalled known-length upload is bounded and never reframed as chunked.
        assert b"502" in raw_request(b"POST /upload HTTP/1.1\r\nHost: public\r\nContent-Length: 100\r\n\r\nx")
        with socket.create_connection(("localhost", 8002), 3) as plain:
            plain.settimeout(2)
            time.sleep(0.5)
            assert plain.recv(1) == b""  # public TLS handshake limit
        with ssl._create_unverified_context().wrap_socket(socket.create_connection(("localhost", 8002), 3), server_hostname="localhost") as slow:
            slow.settimeout(2)
            slow.sendall(b"GET / HTTP/1.1\r\n")
            time.sleep(0.5)
            answer = slow.recv(4096)
            assert not answer or b"408" in answer
        upstream.mode = "close"
        assert request(9000, "/ready", secure=False)[0] == 503
        upstream.mode = "normal"
        assert request(9000, "/ready", secure=False)[0] == 200
        assert request(9000, "/live", secure=False)[0] == 200
        # In-flight work completes within drain; sockets stop accepting new work.
        upstream.mode = "drain"
        answer = []
        worker = threading.Thread(target=lambda: answer.append(request(8002)))
        worker.start()
        time.sleep(0.08)
        gateway.close()
        worker.join(3)
        assert answer[0][0] == 200
    finally:
        gateway.close()
        upstream.close()
    log = (directory / "gateway.log").read_text()
    for secret in ["secret-cookie", "secret=bytes", "raw%2fkeep", "add=0001", "password-secret", "spoofed"]:
        assert secret not in log
    for line in log.splitlines():
        record = json.loads(line)
        if "request_id" in record.get("fields", {}):
            assert set(record["fields"]) == {"request_id", "method", "route", "status", "duration_ms"}
    # Wrong SAN, wrong CA, unavailable origin, and drain deadline use real process entrypoints.
    for ca, host, expected in [(wrong_cert, "localhost", 502), (upstream_cert, "127.0.0.1", 502)]:
        fake = SyntheticUpstream(upstream_key, upstream_cert)
        gateway = Gateway(binary, directory, configuration(key, cert, ca, f"https://{host}:{fake.port}"))
        try:
            wait_for(lambda: request(9000, "/live", secure=False)[0] == 200)
            assert request(8002)[0] == expected
            assert request(9000, "/ready", secure=False)[0] == 503
        finally:
            gateway.close()
            fake.close()
    gateway = Gateway(binary, directory, configuration(key, cert, upstream_cert, f"https://localhost:{upstream.port}"))
    try:
        wait_for(lambda: request(9000, "/live", secure=False)[0] == 200)
        assert request(8002)[0] == 502
    finally:
        gateway.close()
    fake = SyntheticUpstream(upstream_key, upstream_cert)
    settings = configuration(key, cert, upstream_cert, f"https://localhost:{fake.port}")
    settings = settings.replace('trusted_proxy_cidrs = []', 'trusted_proxy_cidrs = ["127.0.0.0/8"]')
    settings = settings.replace('connect_timeout_ms = 300', 'connect_timeout_ms = 3000')
    settings = settings.replace('response_timeout_ms = 700', 'response_timeout_ms = 5000')
    settings = settings.replace('shutdown_drain_ms = 1000', 'shutdown_drain_ms = 200')
    gateway = Gateway(binary, directory, settings)
    sockets = []
    try:
        wait_for(lambda: request(9000, "/live", secure=False)[0] == 200)
        assert request(8002, headers={"X-Forwarded-For": "192.0.2.1", "Forwarded": "spoofed"})[0] == 200
        head = fake.requests[-1][0].lower()
        assert b"x-forwarded-for: 192.0.2.1, 127.0.0.1" in head and b"spoofed" not in head
        request(8002, headers={"X-Forwarded-For": "invalid"})
        assert b"x-forwarded-for: 127.0.0.1" in fake.requests[-1][0].lower()
        # TLS handshake work is capped; management remains responsive under saturation.
        for _ in range(128):
            sockets.append(socket.create_connection(("localhost", 8002), 2))
        time.sleep(0.1)
        with socket.create_connection(("localhost", 8002), 2) as excess:
            excess.settimeout(2)
            assert excess.recv(1) == b""
        assert request(9000, "/live", secure=False)[0] == 200
        for connection in sockets:
            connection.close()
        sockets.clear()
        time.sleep(0.1)
        fake.mode = "hold"
        with ssl._create_unverified_context().wrap_socket(socket.create_connection(("localhost", 8002), 3), server_hostname="localhost") as client:
            client.settimeout(2)
            client.sendall(b"GET /hold HTTP/1.1\r\nHost: public\r\n\r\n")
            wait_for(lambda: any(b"GET /hold" in head for head, _ in fake.requests))
            started = time.monotonic()
            gateway.close()
            assert time.monotonic() - started < 1.5
            assert not client.recv(1)
    finally:
        for connection in sockets:
            connection.close()
        gateway.close()
        fake.close()
    print("PASS: native CLI/config, raw transport, framing, retries, faults, cancellation, deadlines, readiness, shutdown, log redaction")


def corpus(port):
    cookie = None
    results = []

    def call(path, method="GET", form=None):
        nonlocal cookie
        headers = {"Host": "public.example:8002"}
        if cookie:
            headers["Cookie"] = cookie
        if form is not None:
            headers["Content-Type"] = "application/x-www-form-urlencoded"
        status, response_headers, body = request(port, path, method, form, headers)
        kept = []
        for name, value in response_headers:
            name = name.lower()
            if name == "set-cookie":
                match = re.search(r"SESSID=([^;]+)", value)
                if match:
                    cookie = "SESSID=" + match[1]
                value = re.sub(r"SESSID=[^;]+", "SESSID=<opaque>", value)
            if name in ("location", "set-cookie", "content-type"):
                kept.append((name, value))
        # Only /info is excluded: its connection/TLS/forwarding metadata intentionally differs.
        results.append((status, kept, body))
        return status, body

    for path in ["/", "/hello", "/unknown", "/files/main.css", "/app/login", "/app/main"]:
        call(path)
    for method in ["HEAD", "PUT", "OPTIONS", "CUSTOM"]:
        call("/hello", method)
    call("/app/login", "POST", "user=alice&password=bad")
    call("/app/login?err")
    call("/app/register", "POST", "user=alice&name=Alice&password1=secret&password2=other")
    status, body = call("/app/register?err=2")
    assert status == 200 and b"Alice" in body
    call("/app/register", "POST", "user=alice&name=Alice&password1=secret&password2=secret")
    call("/app/main")
    call("/app/logout")
    call("/app/login", "POST", "user=alice&password=secret")
    call("/app/account")
    call("/app/account/edit", "POST", "name=Retained&password1=one&password2=two")
    _, body = call("/app/account/edit?err=2")
    assert b"Retained" in body
    call("/app/account/edit", "POST", "name=Updated&password1=&password2=")
    call("/app/account")
    call("/app/shopping")
    call("/app/shopping?_pos=10")
    call("/app/shopping?add=0001")
    call("/app/shopping?add=0001")
    _, body = call("/app/cart")
    assert b"53.34" in body
    call("/app/cart?del=0001")
    call("/app/cart")
    call("/app/shopping?add=0001")
    call("/app/shopping?add=0001")
    return results, cookie


def verify_docker(args, directory):
    prefix = "eshop-verify-" + str(os.getpid())
    network = prefix + "-network"
    ingress_network = prefix + "-ingress"
    containers = []
    processes = []
    runtime_dirs = []
    volumes = []
    key, cert = certificate(directory, "legacy", "DNS:legacy,DNS:localhost")
    public_key, public_cert = certificate(directory, "container-public", "DNS:localhost")
    try:
        entrypoint = json.loads(command("docker", "image", "inspect", args.legacy_image))[0]["Config"]["Entrypoint"]
        assert entrypoint == ["docker-entrypoint.sh"]
        command("docker", "network", "create", "--internal", network)
        command("docker", "network", "create", ingress_network)
        for name in ["direct", "private"]:
            runtime = directory / name
            runtime.mkdir()
            seed = prefix + "-seed-" + name
            containers.append(seed)
            command("docker", "create", "--name", seed, args.legacy_image)
            command("docker", "cp", seed + ":/app/.", str(runtime))
            command("docker", "rm", seed)
            containers.remove(seed)
            (runtime / "private.key").write_bytes(key.read_bytes())
            (runtime / "certificate.crt").write_bytes(cert.read_bytes())
            runtime_dirs.append(runtime)
            volume = prefix + "-runtime-" + name
            volumes.append(volume)
            command("docker", "volume", "create", volume)
            holder = prefix + "-holder-" + name
            containers.append(holder)
            command("docker", "create", "--name", holder, "--mount", f"type=volume,src={volume},dst=/data",
                    "--entrypoint", "/bin/true", args.legacy_image)
            command("docker", "cp", str(runtime) + "/.", holder + ":/data")
            command("docker", "rm", holder)
            containers.remove(holder)
            container = prefix + "-" + name
            containers.append(container)
            publish = ["-p", "127.0.0.1:18002:8002"] if name == "direct" else []
            command("docker", "run", "-d", "--name", container, "--network", ingress_network if name == "direct" else network,
                    "--network-alias", "legacy" if name == "private" else "direct", *publish,
                    "--mount", f"type=volume,src={volume},dst=/app", args.legacy_image)
        direct, private = prefix + "-direct", prefix + "-private"
        wait_for(lambda: request(18002)[2] == b"Hello!")
        # Exercise the native entrypoint against the real legacy runtime first.
        native = Gateway(args.binary, directory, configuration(public_key, public_cert, cert, "https://localhost:18002"))
        processes.append(native)
        wait_for(lambda: request(9000, "/ready", secure=False)[0] == 200)
        assert request(8002)[2] == b"Hello!"
        native.close()
        processes.remove(native)
        # Separate gateway image: only TLS/config mounts, no management or legacy port publication.
        tls = directory / "tls"
        tls.mkdir()
        for source, target in [(public_key, "public.key"), (public_cert, "public.crt"), (cert, "legacy-ca.crt")]:
            (tls / target).write_bytes(source.read_bytes())
            (tls / target).chmod(0o644)
        config = directory / "container.toml"
        config.write_text((Path(__file__).parent / "gateway.example.toml").read_text())
        config.chmod(0o644)
        directory.chmod(0o755)
        inputs = prefix + "-inputs"
        volumes.append(inputs)
        command("docker", "volume", "create", inputs)
        holder = prefix + "-inputs-holder"
        containers.append(holder)
        command("docker", "create", "--name", holder, "--mount", f"type=volume,src={inputs},dst=/inputs",
                "--entrypoint", "/bin/true", args.legacy_image)
        command("docker", "cp", str(tls), holder + ":/inputs/tls")
        command("docker", "cp", str(config), holder + ":/inputs/gateway.toml")
        command("docker", "rm", holder)
        containers.remove(holder)
        # The example uses /tls; volume root here contains only gateway inputs.
        config.write_text(config.read_text().replace('"/tls/', '"/inputs/tls/'))
        holder = prefix + "-inputs-holder"
        containers.append(holder)
        command("docker", "create", "--name", holder, "--mount", f"type=volume,src={inputs},dst=/inputs",
                "--entrypoint", "/bin/true", args.legacy_image)
        command("docker", "cp", str(config), holder + ":/inputs/gateway.toml")
        command("docker", "rm", holder)
        containers.remove(holder)
        gateway_name = prefix + "-gateway"
        def start_gateway():
            containers.append(gateway_name)
            command("docker", "run", "-d", "--name", gateway_name, "--network", network,
                    "-p", "127.0.0.1:8002:8002", "--read-only", "--cap-drop", "ALL",
                    "--mount", f"type=volume,src={inputs},dst=/inputs,readonly",
                    args.gateway_image, "--config", "/inputs/gateway.toml")
            command("docker", "network", "connect", ingress_network, gateway_name)
            wait_for(lambda: request(8002)[2] == b"Hello!")
        start_gateway()
        inspect = json.loads(command("docker", "inspect", gateway_name, private))
        assert all(mount["Destination"] != "/app" for mount in inspect[0]["Mounts"])
        assert set(inspect[0]["HostConfig"]["PortBindings"]) == {"8002/tcp"}
        assert not inspect[1]["HostConfig"]["PortBindings"]
        direct_results, _ = corpus(18002)
        proxy_results, cookie = corpus(8002)
        assert direct_results == proxy_results, "compatibility corpus mismatch"
        assert b"HTTP_X_FORWARDED" in request(8002, "/info")[2]
        # Replacement owns no application state and must leave session and DBFs intact.
        writer_started = json.loads(command("docker", "inspect", private))[0]["State"]["StartedAt"]
        command("docker", "stop", "-t", "5", gateway_name)
        command("docker", "rm", gateway_name)
        containers.remove(gateway_name)
        start_gateway()
        assert b"53.34" in request(8002, "/app/cart", headers={"Cookie": cookie})[2]
        assert json.loads(command("docker", "inspect", private))[0]["State"]["StartedAt"] == writer_started
        # Quiesced rollback: stop gateway, stop sole writer using the existing lifecycle,
        # retain full runtime; recreate direct ingress, never seed a replacement.
        command("docker", "stop", "-t", "5", gateway_name)
        command("docker", "rm", gateway_name)
        containers.remove(gateway_name)
        command("docker", "exec", private, "./eshop", "//stop")
        command("docker", "wait", private)
        command("docker", "cp", private + ":/app/.", str(runtime_dirs[1]))
        digest = lambda runtime: {name: hashlib.sha256((runtime / name).read_bytes()).hexdigest() for name in ["users.dbf", "carts.dbf", "items.dbf"]}
        stored = digest(runtime_dirs[1])
        command("docker", "rm", private)
        containers.remove(private)
        fallback = prefix + "-fallback"
        containers.append(fallback)
        command("docker", "run", "-d", "--name", fallback, "-p", "127.0.0.1:8002:8002",
                "--mount", f"type=volume,src={prefix}-runtime-private,dst=/app", args.legacy_image)
        wait_for(lambda: request(8002)[2] == b"Hello!")
        command("docker", "cp", fallback + ":/app/.", str(runtime_dirs[1]))
        assert digest(runtime_dirs[1]) == stored
        assert request(8002, "/app/cart", headers={"Cookie": cookie})[0] == 303
        status, headers, _ = request(8002, "/app/login", "POST", "user=alice&password=secret",
                                     {"Content-Type": "application/x-www-form-urlencoded"})
        assert status == 303 and ("Location", "main") in headers
        new_cookie = next(value.split(';')[0] for name, value in headers if name.lower() == "set-cookie")
        assert b"53.34" in request(8002, "/app/cart", headers={"Cookie": new_cookie})[2]
        assert b"Updated" in request(8002, "/app/account", headers={"Cookie": new_cookie})[2]
        command("docker", "exec", fallback, "./eshop", "//stop")
        command("docker", "wait", fallback)
        command("docker", "rm", fallback)
        containers.remove(fallback)
        activation_log = (directory / "activation.log").open("wb")
        activation = subprocess.Popen([str(Path(__file__).parent / "run-gateway.sh"),
                "--activate-ingress-volumes", args.legacy_image, args.gateway_image,
                prefix + "-runtime-private", inputs], stdout=activation_log, stderr=activation_log,
                start_new_session=True)
        try:
            wait_for(lambda: request(8002)[2] == b"Hello!")
            # An explicit activation against an already-owned runtime is refused.
            refused = subprocess.run([str(Path(__file__).parent / "run-gateway.sh"),
                    "--activate-ingress-volumes", args.legacy_image, args.gateway_image,
                    prefix + "-runtime-private", inputs], capture_output=True, timeout=15)
            assert refused.returncode != 0 and b"already belongs" in refused.stderr
        finally:
            os.killpg(activation.pid, signal.SIGTERM)
            activation.wait(timeout=45)
            activation_log.close()
        assert not command("docker", "ps", "-aq", "--filter", "name=eshop-ingress-")
        assert not command("docker", "network", "ls", "-q", "--filter", "name=eshop-ingress-")
        print("PASS: legacy/image and native entrypoints, independent compatibility corpus, private exposure, gateway replacement, quiesced DBF rollback, explicit activation/cleanup; Harbour restart loses sessions as expected")
    finally:
        for process in processes:
            process.close()
        for container in reversed(containers):
            if sys.exc_info()[0] is not None:
                result = subprocess.run(["docker", "logs", "--tail", "8", container], capture_output=True, timeout=10)
                print(container, result.stdout.decode(errors="replace"), result.stderr.decode(errors="replace"))
            subprocess.run(["docker", "rm", "-f", container], capture_output=True, timeout=45)
        subprocess.run(["docker", "network", "rm", network], capture_output=True, timeout=15)
        subprocess.run(["docker", "network", "rm", ingress_network], capture_output=True, timeout=15)
        for volume in volumes:
            subprocess.run(["docker", "volume", "rm", volume], capture_output=True, timeout=15)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--legacy-image")
    parser.add_argument("--gateway-image")
    args = parser.parse_args()
    args.binary = str(Path(args.binary).resolve())
    if bool(args.legacy_image) != bool(args.gateway_image):
        parser.error("both images are required for Docker verification")
    with tempfile.TemporaryDirectory(prefix="eshop-gateway-verify-") as temporary:
        directory = Path(temporary)
        verify_transport(args.binary, directory)
        if args.legacy_image:
            verify_docker(args, directory)


if __name__ == "__main__":
    main()
