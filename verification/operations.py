"""Exercise real local serve/probe commands against controlled TLS transport faults."""
import concurrent.futures
import http.client
import os
from pathlib import Path
import socket
import ssl
import subprocess
import threading
import time

TMP = Path(os.environ["CHECK_TMP"])
BINARY = os.environ.get("GATEWAY_BINARY", "gateway/target/release/eshop-gateway")


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def main():
    public = free_port()
    private = free_port()
    env = dict(os.environ, GATEWAY_BIND=f"127.0.0.1:{public}",
               LEGACY_UPSTREAM_URL=f"https://localhost:{private}",
               GATEWAY_TLS_CERT_FILE=str(TMP / "public.crt"), GATEWAY_TLS_KEY_FILE=str(TMP / "public.key"),
               LEGACY_TLS_CA_FILE=str(TMP / "public.crt"), GATEWAY_CONNECT_SECONDS="1",
               GATEWAY_EXCHANGE_SECONDS="2", GATEWAY_DRAIN_SECONDS="1")
    for changes in [{"GATEWAY_ACTIVE_FAMILIES": "cart"}, {"LEGACY_UPSTREAM_URL": "https://secret:password@localhost"},
                    {"GATEWAY_BIND": "bad"}, {"GATEWAY_EXCHANGE_SECONDS": "0"},
                    {"GATEWAY_TLS_KEY_FILE": str(TMP / "private.key")}, {"GATEWAY_LOG_LEVEL": "invalid"},
                    {"LEGACY_TLS_CA_FILE": "/missing"}]:
        result = subprocess.run([BINARY, "serve"], env=dict(env, **changes), capture_output=True, timeout=5)
        assert result.returncode != 0
        assert b"password" not in result.stderr
    log = (TMP / "local.log").open("wb")
    process = subprocess.Popen([BINARY, "serve"], env=env, stdout=log, stderr=log)
    counts = {}
    ready = threading.Event()
    stopping = threading.Event()
    workers = concurrent.futures.ThreadPoolExecutor(max_workers=8)

    def request(path):
        conn = http.client.HTTPSConnection("localhost", public,
                    context=ssl.create_default_context(cafile=str(TMP / "public.crt")), timeout=5)
        conn.request("GET", path, headers={"Host": "sensitive-host", "Cookie": "secret-cookie"})
        response = conn.getresponse()
        try:
            return response.status, response.getheaders(), response.read()
        finally:
            conn.close()

    def handle(tcp):
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(TMP / "public.crt", TMP / "public.key")
        try:
            with context.wrap_socket(tcp, server_side=True) as tls:
                data = b""
                while b"\r\n\r\n" not in data:
                    data += tls.recv(4096)
                path = data.split(b" ")[1].decode()
                counts[path] = counts.get(path, 0) + 1
                assert b"sensitive-host" in data or path in {"/hello", "/slow"}
                if path == "/headers":
                    tls.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nConnection: close, x-hop\r\nx-hop: gone\r\nLocation: relative\r\n\r\nOK")
                elif path == "/truncate":
                    tls.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort")
                elif path == "/slow-body":
                    tls.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nx")
                    time.sleep(3)
                elif path == "/slow":
                    time.sleep(3)
                elif path == "/brief":
                    time.sleep(.4)
                    tls.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
                elif path == "/mutate?add=0001":
                    pass  # Command accepted then transport fails: MUST NOT replay.
                else:
                    tls.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nHello!")
        except (OSError, ssl.SSLError):
            tcp.close()

    def upstream():
        with socket.socket() as listener:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind(("127.0.0.1", private))
            listener.listen()
            listener.settimeout(.2)
            ready.set()
            while not stopping.is_set():
                try:
                    tcp, _ = listener.accept()
                    workers.submit(handle, tcp)
                except socket.timeout:
                    pass

    try:
        for _ in range(30):
            if subprocess.run([BINARY, "probe", "live"], env=env, capture_output=True).returncode == 0:
                break
            time.sleep(.1)
        assert request("/hello")[0] == 502
        assert subprocess.run(["cargo", "run", "--locked", "--manifest-path", "gateway/Cargo.toml", "--", "probe", "live"],
                              env=env, capture_output=True, timeout=120).returncode == 0
        assert subprocess.run([BINARY, "probe", "ready"], env=env, capture_output=True).returncode != 0
        thread = threading.Thread(target=upstream)
        thread.start()
        assert ready.wait(3)
        assert subprocess.run([BINARY, "probe", "ready"], env=env, capture_output=True).returncode == 0
        result = request("/headers")
        assert result[0] == 200 and result[2] == b"OK"
        assert len([field for field in result[1] if field[0].lower() == "set-cookie"]) == 2
        assert "x-hop" not in dict(result[1])
        # Request reuse explicitly; inspect the original TLS socket so the client
        # cannot silently reconnect and hide a connection-age deadline regression.
        context = ssl.create_default_context(cafile=str(TMP / "public.crt"))
        with context.wrap_socket(socket.create_connection(("localhost", public)), server_hostname="localhost") as tls:
            tls.settimeout(5)
            tls.sendall(b"GET /hello HTTP/1.1\r\nHost: sensitive-host\r\nConnection: keep-alive\r\n\r\n")
            response = http.client.HTTPResponse(tls)
            try:
                response.begin()
                assert response.status == 200 and response.read() == b"Hello!"
                assert response.getheader("Connection", "").lower() == "close"
            finally:
                response.close()
            assert tls.recv(1) == b"", "public connection remained reusable"
        assert request("/mutate?add=0001")[0] == 502
        assert counts["/mutate?add=0001"] == 1
        assert request("/slow")[0] == 504
        for path in ["/truncate", "/slow-body"]:
            try:
                request(path)
            except (http.client.IncompleteRead, OSError):
                pass
            else:
                raise AssertionError("failed response body was accepted")
        # Incomplete known-length upload must expire, not hang or gain new framing.
        context = ssl.create_default_context(cafile=str(TMP / "public.crt"))
        with context.wrap_socket(socket.create_connection(("localhost", public)), server_hostname="localhost") as tls:
            tls.settimeout(5)
            tls.sendall(b"POST /slow HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\nx")
            assert b"504" in tls.recv(4096)
        # Bound slow TLS handshakes without retaining unbounded connections/tasks.
        sockets = [socket.create_connection(("localhost", public)) for _ in range(270)]
        time.sleep(1.3)
        for tcp in sockets:
            tcp.close()
        assert request("/hello")[2] == b"Hello!"
        graceful = workers.submit(request, "/brief")
        time.sleep(.1)
        process.terminate()
        assert graceful.result(timeout=3)[2] == b"OK"
        process.wait(timeout=3)
        assert process.returncode == 0
        process = subprocess.Popen([BINARY, "serve"], env=env, stdout=log, stderr=log)
        for _ in range(30):
            if subprocess.run([BINARY, "probe", "live"], env=env, capture_output=True).returncode == 0:
                break
            time.sleep(.1)
        # Graceful completion and bounded forced drain are tested separately.
        future = workers.submit(request, "/slow-body")
        time.sleep(.2)
        before = time.monotonic()
        process.terminate()
        process.wait(timeout=3)
        assert time.monotonic() - before < 2
        try:
            future.result(timeout=3)
        except (http.client.HTTPException, OSError):
            pass
        assert process.returncode == 0
        log.flush()
        content = (TMP / "local.log").read_text()
        assert all(secret not in content for secret in ["secret-cookie", "sensitive-host", "?add=0001"])
        print("PASS: local entrypoints, config, startup ordering, explicit close on reuse, deadlines, truncation, no GET retries, graceful/forced drain, connection limits")
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=4)
        stopping.set()
        if 'thread' in locals():
            thread.join(timeout=3)
        workers.shutdown(wait=True)
        log.close()


if __name__ == "__main__":
    main()
