#!/usr/bin/env python3
"""Bounded, isolated bootstrap checks; only Python stdlib, OpenSSL and Docker."""
import argparse
import contextlib
import difflib
import http.client
import http.cookies
import os
from pathlib import Path
import re
import socket
import ssl
import subprocess
import tempfile
import time
import uuid


def command(*args, timeout=120):
    return subprocess.run(args, check=True, capture_output=True, text=True,
                          timeout=timeout).stdout.strip()


def request(port, path, method="GET", data=None, cookies=None, headers=None,
            secure=True, chunked=False):
    context = ssl._create_unverified_context()  # Test public self-signed endpoint only.
    connection = (http.client.HTTPSConnection("127.0.0.1", port, context=context, timeout=8)
                  if secure else http.client.HTTPConnection("127.0.0.1", port, timeout=8))
    fields = {"Host": "localhost:8002", "Connection": "close"}
    fields.update(headers or {})
    if cookies:
        fields["Cookie"] = "; ".join(f"{key}={value}" for key, value in cookies.items())
    if data is not None:
        fields.setdefault("Content-Type", "application/x-www-form-urlencoded")
    try:
        connection.request(method, path, body=data, headers=fields, encode_chunked=chunked)
        reply = connection.getresponse()
        result = reply.status, reply.getheaders(), reply.read()
        if cookies is not None:
            for name, value in result[1]:
                if name.lower() == "set-cookie":
                    parsed = http.cookies.SimpleCookie(value)
                    for key, morsel in parsed.items():
                        if morsel.value:
                            cookies[key] = morsel.value
                        else:
                            cookies.pop(key, None)
        return result
    finally:
        connection.close()


def stable(reply, method="GET"):
    status, headers, data = reply
    selected = []
    for name, value in headers:
        name = name.lower()
        if name in {"content-type", "location", "last-modified", "content-encoding", "set-cookie"}:
            if name == "set-cookie":
                value = re.sub(r"SESSID=[^;]*", "SESSID=<opaque>", value)
            selected.append((name, value))
    return status, sorted(selected), b"" if method == "HEAD" else data


def wait(check):
    deadline = time.monotonic() + 40
    while time.monotonic() < deadline:
        try:
            if check():
                return
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(0.25)
    raise AssertionError("readiness deadline exceeded")


class Harness:
    def __init__(self, args, directory):
        self.args, self.directory = args, directory
        self.prefix = "eshop-check-" + uuid.uuid4().hex[:10]
        self.containers, self.volumes = [], []
        self.network = self.prefix

    def volume(self, suffix):
        name = self.prefix + "-" + suffix
        command("docker", "volume", "create", name)
        self.volumes.append(name)
        return name

    def create(self, suffix, image, *options):
        name = self.prefix + "-" + suffix
        command("docker", "create", "--name", name, "--network", self.network,
                *options, image)
        self.containers.append(name)
        return name

    def port(self, container, port):
        return int(command("docker", "port", container, str(port)).rsplit(":", 1)[1])

    def certificate(self, name, san):
        key, cert = self.directory / (name + ".key"), self.directory / (name + ".crt")
        command("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                "-subj", "/CN=" + name, "-addext", "subjectAltName=" + san,
                "-addext", "basicConstraints=critical,CA:FALSE",
                "-keyout", str(key), "-out", str(cert))
        key.chmod(0o644)  # Disposable test keys; production permissions are operator-owned.
        return key, cert

    def run(self):
        command("docker", "network", "create", self.network)
        upstream_key, upstream_cert = self.certificate("legacy", "DNS:legacy,DNS:localhost")
        public_key, public_cert = self.certificate("public", "DNS:localhost")
        direct_volume, proxy_volume, tls_volume = self.volume("direct"), self.volume("proxy"), self.volume("tls")
        direct = self.create("direct", self.args.legacy_image, "-v", direct_volume + ":/app", "-p", "127.0.0.1::8002")
        legacy = self.create("legacy", self.args.legacy_image, "--network-alias", "legacy", "--network-alias", "wrongname", "-v", proxy_volume + ":/app")
        for container in [direct, legacy]:
            command("docker", "cp", str(upstream_key), container + ":/app/private.key")
            command("docker", "cp", str(upstream_cert), container + ":/app/certificate.crt")
            command("docker", "start", container)
        settings = ["PUBLIC_CERT=/tls/public.crt", "PUBLIC_KEY=/tls/public.key", "LEGACY_TRUST=/tls/legacy.crt",
                    "LEGACY_UPSTREAM=https://legacy:8002", "MANAGEMENT_BIND=0.0.0.0:9000",
                    "CONNECT_SECONDS=2", "HEADER_SECONDS=2", "BODY_IDLE_SECONDS=2", "SHUTDOWN_SECONDS=3"]
        options = ["-v", tls_volume + ":/tls:ro", "-p", "127.0.0.1::8002", "-p", "127.0.0.1::9000"]
        for setting in settings:
            options += ["-e", setting]
        staging = self.create("staging", self.args.legacy_image, "--entrypoint", "/bin/true", "-v", tls_volume + ":/tls")
        for source, destination in [(public_cert, "public.crt"), (public_key, "public.key"), (upstream_cert, "legacy.crt")]:
            command("docker", "cp", str(source), staging + ":/tls/" + destination)
        gateway = self.create("gateway", self.args.gateway_image, *options)
        command("docker", "start", gateway)
        direct_port, proxy_port, health_port = self.port(direct, 8002), self.port(gateway, 8002), self.port(gateway, 9000)
        wait(lambda: request(health_port, "/ready", secure=False)[0] == 200)
        wait(lambda: request(direct_port, "/hello")[2] == b"Hello!")
        assert command("docker", "port", legacy) == ""
        mounts = command("docker", "inspect", "--format", "{{json .Mounts}}", gateway)
        assert proxy_volume not in mounts and direct_volume not in mounts
        assert command("docker", "exec", legacy, "test", "!", "-e", "/app/.uhttpd.stop") == ""

        checks = 0
        def compare(path, method="GET", data=None, jars=None, headers=None):
            nonlocal checks
            jars = jars or [None, None]
            replies = [request(port, path, method, data, jar, headers) for port, jar in zip([direct_port, proxy_port], jars)]
            assert stable(replies[0], method) == stable(replies[1], method), (path, method, stable(replies[0], method), stable(replies[1], method))
            checks += 1
            return replies

        for path in ["/hello", "/", "/app/login", "/app/login?err", "/app/main", "/app/cart", "/missing", "/hello/", "/files/main.css"]:
            compare(path)
        for method in ["HEAD", "OPTIONS", "PUT", "DELETE"]:
            compare("/hello", method)
        css = compare("/files/main.css")[0]
        modified = dict((name.lower(), value) for name, value in css[1]).get("last-modified")
        assert modified
        compare("/files/main.css", headers={"If-Modified-Since": modified})
        jars = [{}, {}]
        compare("/app/register", "POST", b"user=bootstrap&name=Retained&password1=x&password2=y", jars)
        compare("/app/register?err=2", jars=jars)
        compare("/app/register", "POST", b"user=bootstrap&name=Bootstrap&password1=secret&password2=secret", jars)
        compare("/app/main", jars=jars)
        for path in ["/app/shopping?add=0001", "/app/shopping?add=0001", "/app/cart", "/app/cart?del=0001", "/app/cart", "/app/shopping?_pos=10"]:
            compare(path, jars=jars)
        compare("/app/logout", jars=jars)
        compare("/app/main", jars=jars)
        compare("/app/login", "POST", b"user=bootstrap&password=wrong", jars)
        compare("/app/login", "POST", b"user=bootstrap&password=secret", jars)
        for path in ["/app/cart", "/app/cart?del=0001", "/app/cart", "/app/shopping?add=0001", "/app/cart", "/app/account"]:
            compare(path, jars=jars)
        compare("/app/account/edit", "POST", b"name=Retry&password1=x&password2=y", jars)
        compare("/app/account/edit?err=2", jars=jars)
        compare("/app/account/edit", "POST", b"name=Updated&password1=&password2=", jars)
        compare("/app/account", jars=jars)
        compare("/app/register", "POST", b"user=bootstrap&name=Exists&password1=x&password2=x", [{}, {}])

        # Metadata is compared row-by-row, with only socket and framing exceptions.
        info_replies = [request(port, "/info") for port in [direct_port, proxy_port]]
        info_text = [reply[2].decode() for reply in info_replies]
        def normalize_info(text):
            return re.sub(r"(<tr><td>(?:REMOTE_ADDR|REMOTE_HOST|REMOTE_PORT|SERVER_ADDR|SERVER_PORT|HTTP_CONNECTION)</td><td>).*?(</td></tr>)", r"\1<connection>\2", text, flags=re.I | re.S)
        assert normalize_info(info_text[0]) == normalize_info(info_text[1]), "unbounded /info difference\n" + "\n".join(difflib.unified_diff(info_text[0].splitlines(), info_text[1].splitlines()))
        checks += 1

        # Explicit compatibility exception: legacy cannot decode chunked requests.
        raw = b"user=bootstrap&password=secret"
        direct_chunk = request(direct_port, "/app/login", "POST", iter([raw]), chunked=True)
        proxy_chunk = request(proxy_port, "/app/login", "POST", iter([raw]), chunked=True)
        assert dict((key.lower(), value) for key, value in direct_chunk[1])["location"] == "login?err"
        assert dict((key.lower(), value) for key, value in proxy_chunk[1])["location"] == "main"
        assert request(proxy_port, "/hello", "POST", iter([b"x" * (1024 * 1024 + 1)]), chunked=True)[0] == 413

        # STOP/CONT works without a cgroup freezer and retains process-local sessions.
        command("docker", "kill", "--signal", "STOP", legacy)
        try:
            assert request(health_port, "/live", secure=False)[0] == 200
            assert request(health_port, "/ready", secure=False)[0] == 503
            assert request(proxy_port, "/hello")[0] == 504
        finally:
            command("docker", "kill", "--signal", "CONT", legacy)
        wait(lambda: request(health_port, "/ready", secure=False)[0] == 200)
        compare("/app/account", jars=jars)

        # Hold an in-flight known-length request, terminate, then complete it.
        connection = ssl._create_unverified_context().wrap_socket(socket.create_connection(("127.0.0.1", proxy_port), timeout=8), server_hostname="localhost")
        connection.sendall(b"POST /hello HTTP/1.1\r\nHost: localhost:8002\r\nContent-Length: 4\r\nConnection: close\r\n\r\na")
        time.sleep(0.3)
        command("docker", "kill", "--signal", "TERM", gateway)
        connection.sendall(b"bcd")
        result = b""
        while True:
            data = connection.recv(4096)
            if not data:
                break
            result += data
        connection.close()
        assert b"200" in result and b"Hello!" in result, result
        assert command("docker", "wait", gateway, timeout=10) == "0"
        assert command("docker", "inspect", "--format", "{{.State.Running}}", legacy) == "true"
        command("docker", "exec", legacy, "test", "!", "-e", "/app/.uhttpd.stop")
        captured = subprocess.run(["docker", "logs", gateway], check=True, capture_output=True, text=True, timeout=10)
        logs = captured.stdout + captured.stderr
        assert "owner=legacy" in logs and "error=timeout" in logs
        assert all(secret not in logs for secret in ["secret", "SESSID", "password", "bootstrap", "?add="])

        # Invalid activation rejects before binding; registration never enables itself.
        for index, setting in enumerate(["ENABLED_SLICES=hello", "CONNECT_SECONDS=0", "LOG_LEVEL=invalid", "LEGACY_UPSTREAM=http://legacy:8002", "PUBLIC_KEY=/tls/legacy.crt", "LEGACY_TRUST=/tls/missing"]):
            invalid = self.create(f"invalid-{index}", self.args.gateway_image, *options, "-e", setting)
            command("docker", "start", invalid)
            assert command("docker", "wait", invalid, timeout=10) == "1"

        for index, setting in enumerate(["LEGACY_TRUST=/tls/public.crt", "LEGACY_UPSTREAM=https://wrongname:8002"]):
            rejected = self.create(f"tls-{index}", self.args.gateway_image, *options, "-e", setting)
            command("docker", "start", rejected)
            health = self.port(rejected, 9000)
            wait(lambda: request(health, "/live", secure=False)[0] == 200)
            assert request(health, "/ready", secure=False)[0] == 503
            assert request(self.port(rejected, 8002), "/hello")[0] == 502
            command("docker", "stop", "-t", "5", rejected)

        bounded = self.create("bounded", self.args.gateway_image, *options)
        command("docker", "start", bounded)
        wait(lambda: request(self.port(bounded, 9000), "/ready", secure=False)[0] == 200)
        partial = ssl._create_unverified_context().wrap_socket(socket.create_connection(("127.0.0.1", self.port(bounded, 8002)), timeout=8), server_hostname="localhost")
        partial.sendall(b"GET /hello HTTP/1.1\r\nHost:")
        start = time.monotonic()
        reply = partial.recv(4096)
        assert not reply or b"408" in reply
        assert time.monotonic() - start < 5
        partial.close()
        partial = ssl._create_unverified_context().wrap_socket(socket.create_connection(("127.0.0.1", self.port(bounded, 8002)), timeout=8), server_hostname="localhost")
        partial.sendall(b"POST /hello HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\na")
        command("docker", "kill", "--signal", "TERM", bounded)
        assert command("docker", "wait", bounded, timeout=6) == "0"
        partial.close()

        # Same data/certificate inputs, never two Harbour writers. Restart loses sessions.
        command("docker", "stop", "-t", "5", legacy)
        fallback = self.create("fallback", self.args.legacy_image, "-v", proxy_volume + ":/app", "-p", "127.0.0.1::8002")
        command("docker", "start", fallback)
        fallback_port = self.port(fallback, 8002)
        wait(lambda: request(fallback_port, "/hello")[0] == 200)
        lost_session = request(fallback_port, "/app/account", cookies=jars[1])
        assert 300 <= lost_session[0] < 400 and dict((name.lower(), value) for name, value in lost_session[1])["location"] == "/app/login", lost_session
        request(fallback_port, "/app/login", "POST", b"user=bootstrap&password=secret", jars[1])
        assert stable(request(fallback_port, "/app/account", cookies=jars[1])) == stable(request(direct_port, "/app/account", cookies=jars[0]))
        assert stable(request(fallback_port, "/app/cart", cookies=jars[1])) == stable(request(direct_port, "/app/cart", cookies=jars[0]))

        if self.args.native:
            self.native(direct_port, upstream_cert, public_cert, public_key)
        print(f"PASS: {checks} real Harbour comparisons; chunk buffering, outage/recovery, drain, activation rejection, mounts and same-state rollback")

    def native(self, upstream_port, trust, cert, key):
        def free_port():
            with contextlib.closing(socket.socket()) as listener:
                listener.bind(("127.0.0.1", 0))
                return listener.getsockname()[1]
        public, management = free_port(), free_port()
        env = dict(os.environ, PUBLIC_CERT=str(cert), PUBLIC_KEY=str(key), LEGACY_TRUST=str(trust),
                   LEGACY_UPSTREAM=f"https://localhost:{upstream_port}", PUBLIC_BIND=f"127.0.0.1:{public}",
                   MANAGEMENT_BIND=f"127.0.0.1:{management}")
        with tempfile.TemporaryFile() as log:
            process = subprocess.Popen(["cargo", "run", "--locked", "--manifest-path", "gateway/Cargo.toml", "--bin", "eshop-gateway"], env=env, stdout=log, stderr=log)
            try:
                wait(lambda: request(management, "/ready", secure=False)[0] == 200)
                assert request(public, "/hello")[2] == b"Hello!"
                assert stable(request(public, "/")) == stable(request(upstream_port, "/"))
            finally:
                process.terminate()
                try:
                    assert process.wait(timeout=15) == 0
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
                    raise
        print("PASS: cargo run --locked local entrypoint and SIGTERM")

    def cleanup(self):
        for container in reversed(self.containers):
            subprocess.run(["docker", "rm", "-f", container], capture_output=True, timeout=30)
        for volume in reversed(self.volumes):
            subprocess.run(["docker", "volume", "rm", volume], capture_output=True, timeout=30)
        subprocess.run(["docker", "network", "rm", self.network], capture_output=True, timeout=30)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--legacy-image", required=True)
    parser.add_argument("--gateway-image", required=True)
    parser.add_argument("--native", action="store_true", help="also exercise cargo run on the host")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="eshop-cert-") as directory:
        harness = Harness(args, Path(directory))
        try:
            harness.run()
        finally:
            harness.cleanup()


if __name__ == "__main__":
    main()
