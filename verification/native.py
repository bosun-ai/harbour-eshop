"""Exercise the native cargo-run entrypoint against a disposable TLS upstream."""
import argparse
import collections
import http.server
import os
import pathlib
import socket
import ssl
import subprocess
import tempfile
import threading
import time

from boundary import Client, raw


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--tls", required=True)
    args = parser.parse_args()
    tls = pathlib.Path(args.tls)
    counts = collections.Counter()
    observations = []

    class Upstream(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def do_POST(self):
            body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            observations.append((self.path, self.headers, body))
            self.respond(body)

        def do_GET(self):
            counts[self.path] += 1
            if self.path == "/stall":
                time.sleep(5)
            elif self.path == "/partial":
                self.send_response(200)
                self.send_header("Content-Length", "20")
                self.end_headers()
                self.wfile.write(b"short")
                self.wfile.flush()
                self.close_connection = True
                return
            elif self.path == "/slow-response":
                self.send_response(200)
                self.send_header("Content-Length", "20")
                self.end_headers()
                self.wfile.write(b"short")
                self.wfile.flush()
                time.sleep(5)
                return
            elif self.path == "/redirect":
                self.respond(b"redirect", 303, [("Location", "https://unchanged.example/path")])
                return
            self.respond(b"Hello!" if self.path == "/hello" else b"synthetic")

        def respond(self, body, status=200, extra=()):
            self.send_response(status)
            for key, value in [("Content-Length", str(len(body))), ("Content-Type", "application/octet-stream"), ("Set-Cookie", "a=1; Path=/"), ("Set-Cookie", "b=2; Path=/"), ("Connection", "close, X-Remove"), ("X-Remove", "secret"), *extra]:
                self.send_header(key, value)
            self.end_headers()
            self.wfile.write(body)
            self.close_connection = True

    class Server(http.server.ThreadingHTTPServer):
        daemon_threads = True

        def handle_error(self, *_):
            pass  # Expected cancellation/timeout closes the disposable connection.

    server = Server(("0.0.0.0", 0), Upstream)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(tls / "legacy.crt", tls / "legacy.key")
    server.socket = context.wrap_socket(server.socket, server_side=True)
    threading.Thread(target=server.serve_forever, daemon=True).start()

    def free_port():
        with socket.socket() as connection:
            connection.bind(("127.0.0.1", 0))
            return connection.getsockname()[1]

    public, admin = free_port(), free_port()
    env = dict(os.environ, PUBLIC_BIND=f"127.0.0.1:{public}", ADMIN_BIND=f"127.0.0.1:{admin}", PUBLIC_CERT_FILE=str(tls / "public.crt"), PUBLIC_KEY_FILE=str(tls / "public.key"), LEGACY_CA_FILE=str(tls / "ca.crt"), LEGACY_ORIGIN=f"https://localhost:{server.server_port}", CONNECT_TIMEOUT_SECS="1", UPLOAD_TIMEOUT_SECS="2", RESPONSE_TIMEOUT_SECS="3", DRAIN_TIMEOUT_SECS="1")
    client = Client(public, str(tls / "ca.crt"))
    process = None
    with tempfile.TemporaryFile(mode="w+") as logs:
        try:
            process = subprocess.Popen(["cargo", "run", "--locked", "--manifest-path", "gateway/Cargo.toml", "--bin", "eshop-gateway"], env=env, stdout=logs, stderr=logs)
            deadline = time.monotonic() + 30
            while True:
                assert process.poll() is None, "native gateway exited"
                try:
                    if client.request("/hello")[0] == 200:
                        break
                except OSError:
                    pass
                assert time.monotonic() < deadline, "native readiness timeout"
                time.sleep(0.1)
            binary = "gateway/target/debug/eshop-gateway"
            for overrides in [dict(CONNECT_TIMEOUT_SECS="0"), dict(CONNECT_TIMEOUT_SECS="4"), dict(LOG_LEVEL="invalid"), dict(LEGACY_ORIGIN="http://localhost"), dict(PUBLIC_KEY_FILE=str(tls / "missing")), dict(PUBLIC_CERT_FILE=str(tls / "ca.key")), dict(ENABLED_ROUTE_FAMILIES="unknown")]:
                assert subprocess.run([binary], env=dict(env, **overrides), timeout=5, capture_output=True).returncode != 0
            subprocess.run([binary, "healthcheck"], env=env, check=True, timeout=12)
            assert client.request("/redirect")[1] == "https://unchanged.example/path"
            body = {"payload": "x" * 1024 * 1024}
            result = client.request("/encoded%2Fpath?x=%26&x=+", "POST", body)
            assert len(result[4]) == 1024 * 1024 + 8 and len(result[3]) == 2
            path, headers, received = observations[-1]
            assert path == "/encoded%2Fpath?x=%26&x=+"
            assert headers["Host"] == "localhost:8002"
            assert headers["Content-Length"] == str(len(received)) and "Transfer-Encoding" not in headers
            response = raw(public, str(tls / "ca.crt"), b"POST /cookies HTTP/1.1\r\nHost: original\r\nCookie: a=1\r\nCookie: b=2\r\nContent-Length: 3\r\nConnection: close, X-Remove\r\nX-Remove: secret\r\nForwarded: spoof\r\n\r\nabc")
            assert response.count(b"set-cookie:") == 2 and b"x-remove:" not in response.lower()
            _, headers, _ = observations[-1]
            assert headers.get_all("Cookie") == ["a=1", "b=2"] and "Forwarded" not in headers and "X-Remove" not in headers
            assert client.request("/stall")[0] == 504 and counts["/stall"] == 1
            for path in ["/partial", "/slow-response"]:
                try:
                    client.request(path)
                except (OSError, http.client.HTTPException):
                    pass
                else:
                    raise AssertionError(f"{path} incorrectly completed")
                assert counts[path] == 1
            # Slow known-length upload succeeds below its deadline.
            with socket.create_connection(("localhost", public), timeout=8) as tcp:
                with client.context.wrap_socket(tcp, server_hostname="localhost") as connection:
                    connection.sendall(b"POST /slow-upload HTTP/1.1\r\nHost: localhost\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc")
                    time.sleep(0.2)
                    connection.sendall(b"def")
                    assert b"200" in connection.recv(4096)
            # Incomplete uploads and headers cannot occupy listeners indefinitely.
            for prefix in [b"POST /upload-timeout HTTP/1.1\r\nHost: localhost\r\nContent-Length: 10\r\nConnection: close\r\n\r\nx", b"GET /hello HTTP/1.1\r\nHost:"]:
                started = time.monotonic()
                with socket.create_connection(("localhost", public), timeout=6) as tcp:
                    with client.context.wrap_socket(tcp, server_hostname="localhost") as connection:
                        connection.sendall(prefix)
                        result = connection.recv(4096)
                        assert not result or b"502" in result or b"408" in result, result
                assert time.monotonic() - started < 5
            # Cancellation must not replay the upstream request.
            with socket.create_connection(("localhost", public), timeout=8) as tcp:
                with client.context.wrap_socket(tcp, server_hostname="localhost") as connection:
                    connection.sendall(b"GET /stall HTTP/1.1\r\nHost: localhost\r\n\r\n")
                    time.sleep(0.2)
            assert counts["/stall"] == 2
            # Terminate with an actual in-flight request; drain remains bounded.
            pending = socket.create_connection(("localhost", public), timeout=8)
            pending = client.context.wrap_socket(pending, server_hostname="localhost")
            pending.sendall(b"GET /stall HTTP/1.1\r\nHost: localhost\r\n\r\n")
            time.sleep(0.1)
            started = time.monotonic()
            process.terminate()
            assert process.wait(timeout=4) == 0
            assert time.monotonic() - started < 3
            pending.close()
            logs.seek(0)
            output = logs.read()
            assert "payload" not in output and "secret" not in output and "spoof" not in output
            # Trusted material with wrong identity, and unrelated trusted material, fail closed.
            for overrides in [dict(LEGACY_ORIGIN=f"https://127.0.0.2:{server.server_port}"), dict(LEGACY_CA_FILE=str(tls / "public.crt"))]:
                failure_env = dict(env, **overrides)
                process = subprocess.Popen([binary], env=failure_env, stdout=logs, stderr=logs)
                deadline = time.monotonic() + 5
                while True:
                    try:
                        assert client.request("/hello")[0] == 502
                        break
                    except ConnectionRefusedError:
                        assert time.monotonic() < deadline
                        time.sleep(0.05)
                assert subprocess.run([binary, "healthcheck"], env=failure_env, timeout=12, capture_output=True).returncode != 0
                process.terminate()
                assert process.wait(timeout=4) == 0
            print("PASS: native cargo run; large/slow fixed framing, encoded target, duplicate headers, no retries, partial/deadline failures, cancellation, TLS failures, SIGTERM")
        finally:
            if process is not None and process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    main()
