"""Disposable boundary checks, using only Python's standard library and Docker CLI.

No credentials, response bodies, or cookies are written to persistent artifacts.
All subprocesses/requests have deadlines; cleanup runs on failure and signals.
"""

import contextlib
import http.client
import json
import os
from pathlib import Path
import re
import signal
import socket
import ssl
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


ROOT = Path(__file__).resolve().parents[1]


def command(*args, **kwargs):
    return subprocess.run(args, check=True, capture_output=True, text=True,
                          timeout=kwargs.pop("timeout", 90), **kwargs).stdout.strip()


def port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def wait(check, description, seconds=25):
    deadline = time.monotonic() + seconds
    last_error = None
    while time.monotonic() < deadline:
        try:
            if check():
                return
        except (OSError, http.client.HTTPException) as error:
            last_error = type(error).__name__ + ": " + str(error)
        time.sleep(0.15)
    raise AssertionError(f"deadline: {description}; {last_error}")


class Client:
    def __init__(self, endpoint, ca):
        self.port = endpoint
        self.context = ssl.create_default_context(cafile=str(ca))
        self.cookie = ""

    def request(self, path, method="GET", body=None, headers=None, chunked=False):
        connection = http.client.HTTPSConnection("localhost", self.port,
                                                 context=self.context, timeout=8)
        fields = {"Host": "public.example:8002"}
        if self.cookie:
            fields["Cookie"] = self.cookie
        if body is not None:
            fields["Content-Type"] = "application/x-www-form-urlencoded"
        fields.update(headers or {})
        if chunked:
            body = iter([body.encode()])
        try:
            connection.request(method, path, body=body, headers=fields, encode_chunked=chunked)
            response = connection.getresponse()
            values = response.getheaders()
            data = response.read()
            for name, value in values:
                if name.lower() == "set-cookie" and value.startswith("SESSID="):
                    self.cookie = value.split(";", 1)[0]
            return response.status, values, data
        finally:
            connection.close()


def canonical(result):
    status, headers, body = result
    selected = []
    for name, value in headers:
        name = name.lower()
        if name in {"location", "content-type", "set-cookie"}:
            if name == "set-cookie":
                value = re.sub(r"SESSID=[^;]*", "SESSID=<session>", value)
            selected.append((name, value))
    return status, sorted(selected), body


def compatibility(direct, proxied):
    steps = [
        ("/hello", "GET", None), ("/", "GET", None),
        ("/app/main", "GET", None), ("/missing", "GET", None),
        ("/files/main.css", "GET", None), ("/files/missing.css", "GET", None),
        ("/hello", "HEAD", None), ("/hello", "OPTIONS", None),
        ("/app/register", "GET", None),
        ("/app/register", "POST", "user=boundary&name=Boundary&password1=a&password2=b"),
        ("/app/register?err=2", "GET", None),
        ("/app/register", "POST", "user=boundary&name=Boundary&password1=private-password&password2=private-password"),
        ("/app/main", "GET", None),
        ("/app/register", "POST", "user=boundary&name=Boundary&password1=private-password&password2=private-password"),
        ("/app/register?err=3", "GET", None),
        ("/app/logout", "GET", None),
        ("/app/main", "GET", None),
        ("/app/login", "POST", "user=boundary&password=wrong"),
        ("/app/login?err", "GET", None),
        ("/app/login", "POST", "user=boundary&password=private-password"),
        ("/app/shopping", "GET", None),
        ("/app/shopping?_pos=10", "GET", None),
        ("/app/shopping?add=0001", "GET", None),
        ("/app/shopping?add=0001", "GET", None),
        ("/app/cart", "GET", None),
        ("/app/cart?del=0001", "GET", None),
        ("/app/cart", "GET", None),
        ("/app/account", "GET", None),
        ("/app/account/edit", "POST", "name=&password1=&password2="),
        ("/app/account/edit?err=1", "GET", None),
        ("/app/account/edit", "POST", "name=Boundary&password1=a&password2=b"),
        ("/app/account/edit?err=2", "GET", None),
        ("/app/account/edit", "POST", "name=Retained+Name&password1=&password2="),
        ("/app/account", "GET", None),
        ("/app/shopping?add=0002", "GET", None),
        ("/app/cart", "GET", None),
    ]
    for index, (path, method, body) in enumerate(steps):
        left = direct.request(path, method, body)
        right = proxied.request(path, method, body)
        assert canonical(left) == canonical(right), f"compatibility step {index}: {method} {path}"
    assert b"Retained Name" in proxied.request("/app/account")[2]
    assert b"0002" in proxied.request("/app/cart")[2]
    for label, client in [("direct", direct), ("gateway", proxied)]:
        failed = client.request("/app/login", "POST", "user=boundary&password=private-password", chunked=True)
        location = next((value for name, value in failed[1] if name.lower() == "location"), None)
        assert failed[0] == 303 and location == "login?err", f"chunked baseline changed: {label} {failed[0]} {location}"
    left = direct.request("/info")[2]
    right = proxied.request("/info", headers={"X-Forwarded-For": "spoofed-marker"})[2]
    assert left != right and b"spoofed-marker" not in right, "/info must reflect transport differences"
    print(f"PASS compatibility: {len(steps)} paired steps, chunked baseline, /info differences")


class Probe(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    records = []
    lock = threading.Lock()
    hello_started = threading.Event()
    hello_release = threading.Event()
    hold_hello = False

    def log_message(self, *_):
        pass

    def do_POST(self):
        self.do_GET()

    def do_GET(self):
        length = self.headers.get("Content-Length")
        body = self.rfile.read(int(length)) if length else b""
        with self.lock:
            self.records.append((self.path, dict(self.headers), body))
        if self.path == "/ambiguous?add=0001":
            self.connection.shutdown(socket.SHUT_RDWR)
            self.connection.close()
            return
        if self.path in {"/slow", "/drain", "/truncate", "/stall-body"}:
            if self.path == "/stall-body":
                self.send_response(200)
                self.send_header("Content-Length", "100")
                self.end_headers()
                self.wfile.write(b"short")
                self.wfile.flush()
                time.sleep(4)
                return
            if self.path != "/truncate":
                time.sleep(3 if self.path == "/slow" else 0.8)
            else:
                self.send_response(200)
                self.send_header("Content-Length", "100")
                self.end_headers()
                self.wfile.write(b"short")
                self.wfile.flush()
                self.connection.shutdown(socket.SHUT_RDWR)
                self.connection.close()
                return
        if self.path == "/hello":
            if self.hold_hello:
                self.hello_started.set()
                assert self.hello_release.wait(timeout=5), "delayed probe not released"
            payload = b"Hello!"
        else:
            payload = json.dumps({"path": self.path, "headers": dict(self.headers),
                                  "body": body.decode()}).encode()
        self.send_response(302 if self.path == "/redirect" else 200)
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close, x-private")
        self.send_header("X-Private", "remove-me")
        self.send_header("Set-Cookie", "one=1; path=/")
        self.send_header("Set-Cookie", "two=2; HttpOnly")
        if self.path == "/redirect":
            self.send_header("Location", "https://elsewhere.invalid/untouched")
        with contextlib.suppress(OSError):
            self.end_headers()
            self.wfile.write(payload)


def admin_native(endpoint, path):
    connection = http.client.HTTPConnection("127.0.0.1", endpoint, timeout=5)
    try:
        connection.request("GET", path)
        result = connection.getresponse()
        result.read()
        return result.status
    finally:
        connection.close()


def native_checks(directory, ca, cert, key):
    server = ThreadingHTTPServer(("127.0.0.1", 0), Probe)
    server.daemon_threads = True
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    server.socket = context.wrap_socket(server.socket, server_side=True)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    env = os.environ | {
        "PUBLIC_BIND": f"127.0.0.1:{port()}", "ADMIN_BIND": f"127.0.0.1:{port()}",
        "PUBLIC_TLS_CERT": str(cert), "PUBLIC_TLS_KEY": str(key),
        "LEGACY_URL": f"https://localhost:{server.server_port}", "LEGACY_CA_FILE": str(ca),
        "CONNECT_TIMEOUT_SECONDS": "1", "UPSTREAM_TIMEOUT_SECONDS": "2",
        "CLIENT_READ_TIMEOUT_SECONDS": "1", "SHUTDOWN_TIMEOUT_SECONDS": "2", "ENABLED_OWNERS": "",
    }
    binary = str(ROOT / "gateway/target/debug/eshop-gateway")
    log_path = directory / "native.log"
    process = None
    try:
        # Configuration must fail before either socket is opened; values never appear in errors.
        for overrides in [
            {"ENABLED_OWNERS": "unknown"}, {"ENABLED_OWNERS": "cart,cart"},
            {"LEGACY_URL": "https://secret-marker:password@localhost/"},
            {"CONNECT_TIMEOUT_SECONDS": "0"}, {"CLIENT_READ_TIMEOUT_SECONDS": "301"},
            {"LOG_LEVEL": "secret-marker"}, {"PUBLIC_TLS_KEY": str(directory / "legacy.key")},
            {"LEGACY_CA_FILE": str(directory / "empty")},
        ]:
            (directory / "empty").touch()
            result = subprocess.run([binary], env=env | overrides, capture_output=True, timeout=5)
            assert result.returncode != 0 and b"secret-marker" not in result.stderr
        print("PASS invalid configuration and activation fail before listening")

        def start(overrides=None, cargo=False):
            nonlocal process
            log = open(log_path, "ab")
            args = ["cargo", "run", "--locked", "--manifest-path", "gateway/Cargo.toml"] if cargo else [binary]
            process = subprocess.Popen(args, cwd=ROOT, env=env | (overrides or {}), stdout=log, stderr=log)
            log.close()
            wait(lambda: admin_native(int(env["ADMIN_BIND"].split(':')[1]), "/live") == 200, "native liveness")

        def stop():
            nonlocal process
            process.terminate()
            process.wait(timeout=5)
            assert process.returncode == 0
            process = None

        start(cargo=True)
        client = Client(int(env["PUBLIC_BIND"].split(':')[1]), ca)
        admin_port = int(env["ADMIN_BIND"].split(':')[1])
        assert admin_native(admin_port, "/ready") == 200
        result = client.request("/raw%2Fpath?x=%2B&x=two", "POST", "fixed=form", {
            "Connection": "x-remove", "X-Remove": "discard", "Forwarded": "spoofed-marker",
            "X-Forwarded-For": "spoofed-marker", "X-Forwarded-Host": "spoofed-marker",
            "Authorization": "private-authorization", "Cookie": "private-cookie",
        })
        payload = json.loads(result[2])
        headers = {name.lower(): value for name, value in payload["headers"].items()}
        assert payload["path"] == "/raw%2Fpath?x=%2B&x=two"
        assert payload["body"] == "fixed=form" and headers["content-length"] == "10"
        assert headers["host"] == "public.example:8002"
        assert headers["x-forwarded-for"] == "127.0.0.1" and headers["x-forwarded-proto"] == "https"
        assert not {"x-remove", "forwarded", "x-forwarded-host", "transfer-encoding"} & headers.keys()
        assert len([value for name, value in result[1] if name.lower() == "set-cookie"]) == 2
        assert not any(name.lower() == "x-private" for name, _ in result[1])
        redirect = client.request("/redirect")
        assert redirect[0] == 302 and dict((name.lower(), value) for name, value in redirect[1]).get("location") == "https://elsewhere.invalid/untouched"
        for method in ["GET", "POST"]:
            before = len(Probe.records)
            assert client.request("/ambiguous?add=0001", method)[0] == 502
            assert len(Probe.records) == before + 1, "ambiguous request replayed"
        assert client.request("/slow")[0] == 504
        for path in ["/truncate", "/stall-body"]:
            started = time.monotonic()
            try:
                client.request(path)
                raise AssertionError("interrupted response accepted")
            except http.client.IncompleteRead:
                pass
            assert time.monotonic() - started < 4
        # Stalled fixed-length upload must close within the client-read deadline.
        with client.context.wrap_socket(socket.create_connection(("localhost", client.port), timeout=5), server_hostname="localhost") as sock:
            sock.sendall(b"POST /upload HTTP/1.1\r\nHost: public.example\r\nContent-Length: 100\r\n\r\nx")
            started = time.monotonic()
            data = sock.recv(4096)
            assert b"502" in data or b"504" in data
            assert time.monotonic() - started < 4
        print("PASS native cargo run, framing, metadata, cookies, redirects, no replay, bounded failures")
        # In-flight body completes while SIGTERM drains, without stopping the upstream.
        outcome = []
        before = len(Probe.records)
        worker = threading.Thread(target=lambda: outcome.append(client.request("/drain")))
        worker.start()
        wait(lambda: len(Probe.records) > before, "drain request reached upstream")
        stop()
        worker.join(timeout=5)
        assert outcome and outcome[0][0] == 200
        assert server.fileno() >= 0
        print("PASS graceful SIGTERM drain and upstream independence")
        start()
        Probe.hello_started.clear()
        Probe.hello_release.clear()
        Probe.hold_hello = True
        readiness = []
        worker = threading.Thread(target=lambda: readiness.append(admin_native(admin_port, "/ready")))
        worker.start()
        assert Probe.hello_started.wait(timeout=2), "readiness probe did not reach upstream"
        log_offset = log_path.stat().st_size
        process.terminate()
        wait(lambda: '"event":"draining"' in log_path.read_text()[log_offset:],
             "shutdown before releasing readiness probe", seconds=1)
        Probe.hello_release.set()
        worker.join(timeout=5)
        process.wait(timeout=5)
        assert process.returncode == 0 and readiness == [503], readiness
        process = None
        Probe.hold_hello = False
        print("PASS delayed readiness probe returns 503 after SIGTERM begins draining")
        start({"SHUTDOWN_TIMEOUT_SECONDS": "1", "UPSTREAM_TIMEOUT_SECONDS": "10"})
        interrupted = []
        def stalled_request():
            try:
                client.request("/stall-body")
            except (OSError, http.client.HTTPException):
                interrupted.append(True)
        before = len(Probe.records)
        worker = threading.Thread(target=stalled_request)
        worker.start()
        wait(lambda: len(Probe.records) > before, "forced drain request")
        started = time.monotonic()
        stop()
        worker.join(timeout=5)
        assert interrupted and time.monotonic() - started < 3
        print("PASS shutdown deadline terminates a stalled in-flight body")
        for overrides in [
            {"LEGACY_URL": f"https://127.0.0.1:{server.server_port}"},
            {"LEGACY_CA_FILE": str(directory / "other.crt")},
        ]:
            start(overrides)
            assert client.request("/hello")[0] == 502 and admin_native(admin_port, "/ready") == 503
            stop()
        logs = log_path.read_text()
        for secret in ["private-authorization", "private-cookie", "fixed=form", "spoofed-marker", "raw%2Fpath"]:
            assert secret not in logs, "gateway logged secret or raw URI"
        assert '"event":"draining"' in logs
        print("PASS TLS hostname/chain rejection and secret-free gateway logs")
    finally:
        Probe.hold_hello = False
        Probe.hello_release.set()
        if process and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
        server.shutdown()
        server.server_close()


def run():
    prefix = f"eshop-check-{os.getpid()}"
    containers, networks, volumes = [], [], []
    with tempfile.TemporaryDirectory(prefix=prefix) as temporary:
        directory = Path(temporary)
        os.chmod(directory, 0o755)  # Non-root gateway reads only the explicitly mounted TLS directory.
        tls = directory / "tls"
        tls.mkdir(mode=0o755)
        try:
            command("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                    "-addext", "keyUsage=critical,keyCertSign,cRLSign",
                    "-subj", "/CN=Disposable boundary CA", "-keyout", str(directory / "ca.key"), "-out", str(tls / "ca.crt"))
            command("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                    "-addext", "keyUsage=critical,keyCertSign,cRLSign",
                    "-subj", "/CN=Unrelated CA", "-keyout", str(directory / "other.key"), "-out", str(directory / "other.crt"))
            for name in ["localhost", "legacy"]:
                key, csr, cert = directory / f"{name}.key", directory / f"{name}.csr", directory / f"{name}.crt"
                command("openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", f"/CN={name}", "-keyout", str(key), "-out", str(csr))
                extension = directory / "extension"
                extension.write_text(f"subjectAltName=DNS:{name}\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")
                command("openssl", "x509", "-req", "-in", str(csr), "-CA", str(tls / "ca.crt"), "-CAkey", str(directory / "ca.key"),
                        "-CAcreateserial", "-days", "1", "-extfile", str(extension), "-out", str(cert))
            for source, target in [(directory / "localhost.key", tls / "public.key"), (directory / "localhost.crt", tls / "public.crt")]:
                target.write_bytes(source.read_bytes())
                target.chmod(0o644)
            (tls / "ca.crt").chmod(0o644)
            ca = tls / "ca.crt"
            for suffix, internal in [("private", True), ("edge", False)]:
                name = f"{prefix}-{suffix}"
                command("docker", "network", "create", *(["--internal"] if internal else []), name)
                networks.append(name)

            def create(name, *args):
                command("docker", "create", "--name", name, *args)
                containers.append(name)

            # docker cp also works with a daemon whose filesystem differs from the CLI host.
            tls_volume = f"{prefix}-tls"
            command("docker", "volume", "create", tls_volume)
            volumes.append(tls_volume)
            helper = f"{prefix}-tls-copy"
            create(helper, "-v", f"{tls_volume}:/tls", "debian:bookworm-slim", "true")
            command("docker", "cp", str(tls) + "/.", f"{helper}:/tls")
            direct_port, gateway_port = port(), port()
            direct, legacy, gateway = [f"{prefix}-{name}" for name in ["direct", "legacy", "gateway"]]
            create(direct, "-p", f"127.0.0.1:{direct_port}:8002", "harbour-eshop:boundary")
            create(legacy, "--network", networks[0], "--network-alias", "legacy", "harbour-eshop:boundary")
            for name, certname in [(direct, "localhost"), (legacy, "legacy")]:
                command("docker", "cp", str(directory / f"{certname}.crt"), f"{name}:/app/certificate.crt")
                command("docker", "cp", str(directory / f"{certname}.key"), f"{name}:/app/private.key")
            create(gateway, "--network", networks[0], "--network-alias", "gateway",
                   "-p", f"127.0.0.1:{gateway_port}:8002", "-v", f"{tls_volume}:/tls:ro",
                   "-e", "PUBLIC_TLS_CERT=/tls/public.crt", "-e", "PUBLIC_TLS_KEY=/tls/public.key",
                   "-e", "LEGACY_URL=https://legacy:8002", "-e", "LEGACY_CA_FILE=/tls/ca.crt",
                   "-e", "CONNECT_TIMEOUT_SECONDS=2", "-e", "UPSTREAM_TIMEOUT_SECONDS=3", "eshop-gateway:bootstrap")
            command("docker", "network", "connect", networks[1], gateway)
            command("docker", "start", direct, gateway)
            command("docker", "network", "connect", networks[0], direct)
            direct_client, gateway_client = Client(direct_port, ca), Client(gateway_port, ca)
            wait(lambda: direct_client.request("/hello")[0] == 200, "direct startup")
            wait(lambda: gateway_client.request("/hello")[0] == 502, "gateway starts before upstream")
            command("docker", "start", legacy)
            wait(lambda: gateway_client.request("/hello")[0] == 200, "upstream recovery")

            def admin(path):
                output = command("docker", "exec", direct, "bash", "-c",
                                 f"exec 3<>/dev/tcp/gateway/8003; printf 'GET {path} HTTP/1.1\\r\\nHost: admin\\r\\nConnection: close\\r\\n\\r\\n' >&3; cat <&3")
                return int(output.split()[1])

            assert admin("/live") == 200 and admin("/ready") == 200
            assert gateway_client.request("/ready")[0] == direct_client.request("/ready")[0] == 404
            inspected = json.loads(command("docker", "inspect", legacy, gateway))
            assert not inspected[0]["HostConfig"]["PortBindings"]
            assert set(inspected[1]["HostConfig"]["PortBindings"]) == {"8002/tcp"}
            assert inspected[1]["Mounts"][0]["Destination"] == "/tls" and not inspected[1]["Mounts"][0]["RW"]
            assert len(inspected[1]["Mounts"]) == 1 and not (tls / "legacy.key").exists()
            compatibility(direct_client, gateway_client)
            # Restore an authenticated session for rollback and preserve a current cart.
            gateway_client.request("/app/login", "POST", "user=boundary&password=private-password")
            old_cookie = gateway_client.cookie
            command("docker", "stop", "-t", "5", legacy)
            assert admin("/live") == 200 and admin("/ready") == 503
            started = time.monotonic()
            assert gateway_client.request("/hello")[0] == 502 and time.monotonic() - started < 5
            command("docker", "start", legacy)
            wait(lambda: admin("/ready") == 200, "readiness recovery")
            assert dict((name.lower(), value) for name, value in gateway_client.request("/app/main")[1]).get("location") == "/app/login"
            gateway_client.request("/app/login", "POST", "user=boundary&password=private-password")
            assert b"0002" in gateway_client.request("/app/cart")[2]
            print("PASS container startup delay/outage/recovery, private topology, session loss, retained DBFs")
            logs = command("docker", "logs", gateway)
            for secret in ["private-password", "SESSID", "Retained", "add=", "spoofed-marker"]:
                assert secret not in logs
            # Stop publication, quiesce the sole writer, copy the full CURRENT runtime,
            # then recreate that same image with the retained state and public TLS pair.
            command("docker", "stop", "-t", "5", gateway, legacy)
            retained = directory / "retained"
            retained.mkdir()
            command("docker", "cp", f"{legacy}:/app/.", str(retained))
            for name, source in [("certificate.crt", tls / "public.crt"), ("private.key", tls / "public.key")]:
                (retained / name).write_bytes(source.read_bytes())
            image = inspected[0]["Image"]
            restored = f"{prefix}-restored"
            retained_volume = f"{prefix}-retained"
            command("docker", "volume", "create", retained_volume)
            volumes.append(retained_volume)
            create(restored, "-p", f"127.0.0.1:{gateway_port}:8002", "-v", f"{retained_volume}:/app", image)
            command("docker", "cp", str(retained) + "/.", f"{restored}:/app")
            command("docker", "start", restored)
            restored_client = Client(gateway_port, ca)
            wait(lambda: restored_client.request("/hello")[0] == 200, "rollback listener")
            restored_client.cookie = old_cookie
            assert dict((name.lower(), value) for name, value in restored_client.request("/app/main")[1]).get("location") == "/app/login"
            assert restored_client.request("/app/login", "POST", "user=boundary&password=private-password")[0] == 303
            assert b"Retained Name" in restored_client.request("/app/account")[2]
            assert b"0002" in restored_client.request("/app/cart")[2]
            print("PASS same-image rollback to public legacy with current account/cart and invalidated old session")
            native_checks(directory, ca, directory / "localhost.crt", directory / "localhost.key")
        finally:
            for name in reversed(containers):
                subprocess.run(["docker", "rm", "-f", name], capture_output=True, timeout=30)
            for name in reversed(networks):
                subprocess.run(["docker", "network", "rm", name], capture_output=True, timeout=30)
            for name in reversed(volumes):
                subprocess.run(["docker", "volume", "rm", name], capture_output=True, timeout=30)
    print("PASS all boundary checks; temporary containers, networks, state and certificates removed")


if __name__ == "__main__":
    def interrupted(signum, _frame):
        raise SystemExit(f"interrupted by signal {signum}")
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    run()
