#!/usr/bin/env python3
"""Adversarial HTTP/TLS checks using the actual gateway CLI and a test upstream."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import http.client
import json
from pathlib import Path
import signal
import socket
import ssl
import subprocess
import tempfile
import threading
import time

from compatibility import Client, certificate, management, raw, wait, write_config, ROOT


class Upstream:
    def __init__(self, directory):
        self.context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        self.context.load_cert_chain(directory / "upstream.crt", directory / "upstream.key")
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", 8002))
        self.listener.listen()
        self.listener.settimeout(0.1)
        self.stop = threading.Event()
        self.records = []
        self.pool = ThreadPoolExecutor(max_workers=16)
        self.thread = threading.Thread(target=self.accept)
        self.thread.start()

    def accept(self):
        while not self.stop.is_set():
            try:
                stream, _ = self.listener.accept()
                self.pool.submit(self.serve, stream)
            except socket.timeout:
                pass
            except OSError:
                break

    def serve(self, stream):
        try:
            stream.settimeout(3)
            with self.context.wrap_socket(stream, server_side=True) as stream:
                data = b""
                while b"\r\n\r\n" not in data:
                    part = stream.recv(4096)
                    if not part:
                        return
                    data += part
                head, body = data.split(b"\r\n\r\n", 1)
                lines = head.decode().split("\r\n")
                headers = {key.lower(): value for key, value in (line.split(": ", 1) for line in lines[1:])}
                length = int(headers.get("content-length", "0"))
                while len(body) < length:
                    part = stream.recv(length - len(body))
                    if not part:
                        return
                    body += part
                self.records.append((lines[0], headers, body))
                path = lines[0].split(" ")[1]
                if path.startswith("/drop"):
                    return
                if path == "/slow-headers":
                    time.sleep(1)
                if path in ("/partial", "/slow-body"):
                    stream.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nprefix")
                    if path == "/slow-body":
                        time.sleep(1)
                    return
                body = b"Hello!" if path == "/hello" else body or b"opaque\x00bytes"
                stream.sendall(b"HTTP/1.1 201 Created\r\n" if path != "/hello" else b"HTTP/1.1 200 OK\r\n")
                stream.sendall(b"Content-Length: " + str(len(body)).encode() +
                               b"\r\nContent-Type: application/octet-stream\r\nLocation: https://elsewhere.example/unchanged\r\n"
                               b"Set-Cookie: one=1; path=/\r\nSet-Cookie: two=2; HttpOnly\r\n"
                               b"Connection: close, x-hop\r\nx-hop: hidden\r\n\r\n" + body)
        except (OSError, ssl.SSLError):
            stream.close()

    def close(self):
        self.stop.set()
        self.listener.close()
        self.thread.join(timeout=2)
        self.pool.shutdown(wait=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--gateway", default=str(ROOT / "gateway/target/release/eshop-gateway"))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="eshop-transport-") as temporary:
        directory = Path(temporary)
        certificate(directory, "upstream", "DNS:localhost")
        certificate(directory, "public", "DNS:localhost")
        certificate(directory, "wrong", "DNS:wrong.example")
        config = write_config(directory, 18443, 18003, header_ms=300, body_idle_ms=300, deadline_ms=600, drain_ms=400, max_connections=4)
        upstream = Upstream(directory)
        processes = []
        logs = open(directory / "gateway.log", "w+")

        def start(path):
            process = subprocess.Popen([args.gateway, "--config", str(path)], stdout=logs, stderr=logs)
            processes.append(process)
            return process

        try:
            gateway = start(config)
            wait(lambda: management(18003)[0] == 200)
            trust = directory / "public.crt"
            context = ssl.create_default_context(cafile=str(trust))
            connection = http.client.HTTPSConnection("localhost", 18443, context=context, timeout=3)
            connection.request("POST", "/opaque%2Fpath?add=0001&raw=%2f+%26", body=b"a=1&b=%00", headers={
                "Host": "original.example:443", "Cookie": "SESSID=opaque", "Authorization": "secret-token",
                "Connection": "close, x-remove, x-forwarded-for, x-request-id", "x-remove": "hidden", "Forwarded": "for=evil", "X-Forwarded-For": "evil"})
            response = connection.getresponse()
            assert response.status == 201 and response.read() == b"a=1&b=%00"
            assert response.getheader("Location") == "https://elsewhere.example/unchanged"
            assert len(response.headers.get_all("Set-Cookie")) == 2 and response.getheader("x-hop") is None
            record = upstream.records[-1]
            assert record[0] == "POST /opaque%2Fpath?add=0001&raw=%2f+%26 HTTP/1.1"
            assert record[1]["host"] == "original.example:443" and record[1]["cookie"] == "SESSID=opaque"
            assert record[1]["content-length"] == "9" and "transfer-encoding" not in record[1]
            assert "x-remove" not in record[1] and "forwarded" not in record[1]
            assert record[1]["x-forwarded-for"] == "127.0.0.1"
            for path, status in [("/drop?add=0001", 502), ("/slow-headers", 504)]:
                before = len(upstream.records)
                assert Client(18443, trust).request(path, follow=False)[0][0] == status
                time.sleep(0.2)
                assert len(upstream.records) == before + 1, "requests must never retry"
            for path in ["/partial", "/slow-body"]:
                connection = http.client.HTTPSConnection("localhost", 18443, context=context, timeout=3)
                connection.request("GET", path, headers={"Host": "shop.example"})
                response = connection.getresponse()
                assert response.status == 200
                try:
                    response.read()
                    raise AssertionError("partial stream unexpectedly completed")
                except http.client.IncompleteRead as error:
                    assert error.partial == b"prefix", "must not append an error page"
                connection.close()
            for head in [b"Transfer-Encoding: chunked\r\nContent-Length: 0", b"Content-Length: 1\r\nContent-Length: 2",
                         b"Expect: 100-continue", b"Connection: content-length", b"Host: duplicate"]:
                before = len(upstream.records)
                result = raw(18443, trust, b"POST /opaque HTTP/1.1\r\nHost: original.example\r\n" + head + b"\r\nConnection: close\r\n\r\n")
                assert result.startswith(b"HTTP/1.1 400") and len(upstream.records) == before
            # Short/slow bodies cannot complete a legacy mutation or occupy a slot forever.
            result = raw(18443, trust, b"POST /opaque HTTP/1.1\r\nHost: original.example\r\nContent-Length: 5\r\nConnection: close\r\n\r\na")
            assert result.startswith((b"HTTP/1.1 502", b"HTTP/1.1 504"))
            wait(lambda: management(18003)[0] == 200)
            # Clients that cancel release slots and upstream drivers.
            for _ in range(8):
                stream = context.wrap_socket(socket.create_connection(("localhost", 18443), timeout=3), server_hostname="localhost")
                stream.sendall(b"GET /slow-body HTTP/1.1\r\nHost: shop.example\r\n\r\n")
                stream.close()
                time.sleep(0.1)
            wait(lambda: Client(18443, trust).request("/hello")[0][0] == 200)
            # Concurrency bounds include idle TLS clients and free on cancellation.
            held = []
            for _ in range(4):
                held.append(context.wrap_socket(socket.create_connection(("localhost", 18443)), server_hostname="localhost"))
            try:
                extra = socket.create_connection(("localhost", 18443), timeout=2)
                try:
                    context.wrap_socket(extra, server_hostname="localhost")
                    raise AssertionError("connection saturation accepted unbounded work")
                except (OSError, ssl.SSLError):
                    pass
            finally:
                for stream in held:
                    stream.close()
            wait(lambda: Client(18443, trust).request("/hello")[0][0] == 200)
            # Public SAN and trust are independently validated.
            try:
                ssl.create_default_context().wrap_socket(socket.create_connection(("localhost", 18443)), server_hostname="localhost")
                raise AssertionError("untrusted public TLS accepted")
            except ssl.SSLCertVerificationError:
                pass
            wrong = write_config(directory, 18444, 18004, upstream_trust=f'"{directory}/wrong.crt"')
            wrong_process = start(wrong)
            wait(lambda: management(18004, "/livez")[0] == 200)
            assert management(18004)[0] == 503 and Client(18444, trust).request("/hello")[0][0] == 502
            wrong_process.terminate()
            wrong_process.wait(timeout=3)
            trusted = write_config(directory, 18447, 18007, trusted_proxies='["127.0.0.1"]')
            trusted_process = start(trusted)
            wait(lambda: management(18007)[0] == 200)
            connection = http.client.HTTPSConnection("localhost", 18447, context=context, timeout=3)
            connection.request("GET", "/trusted", headers={"Host": "shop.example", "X-Forwarded-For": "192.0.2.10"})
            connection.getresponse().read()
            connection.close()
            assert upstream.records[-1][1]["x-forwarded-for"] == "192.0.2.10"
            trusted_process.terminate()
            trusted_process.wait(timeout=3)
            upstream.context.load_cert_chain(directory / "wrong.crt", directory / "wrong.key")
            san = write_config(directory, 18445, 18005, upstream_trust=f'"{directory}/wrong.crt"')
            san_process = start(san)
            wait(lambda: management(18005, "/livez")[0] == 200)
            assert management(18005)[0] == 503 and Client(18445, trust).request("/hello")[0][0] == 502
            san_process.terminate()
            san_process.wait(timeout=3)
            invalid = write_config(directory, 18446, 18006, enabled_families='["unregistered"]')
            invalid_process = start(invalid)
            assert invalid_process.wait(timeout=3) == 1
            # Bounded drain with a live incomplete request.
            stream = context.wrap_socket(socket.create_connection(("localhost", 18443)), server_hostname="localhost")
            stream.sendall(b"POST /opaque HTTP/1.1\r\nHost: shop.example\r\nContent-Length: 100\r\n\r\nx")
            started = time.monotonic()
            gateway.send_signal(signal.SIGTERM)
            gateway.wait(timeout=2)
            stream.close()
            assert gateway.returncode == 0 and time.monotonic() - started < 2
            logs.flush()
            logs.seek(0)
            text = logs.read()
            for secret in ["secret-token", "SESSID", "add=", "raw=", "evil"]:
                assert secret not in text
            print("PASS: host/raw URI/bytes, fixed length, cookies, hop fields, zero retries, deadlines, partial streams, cancellation, TLS trust/SAN, invalid activation, bounded drain, redaction")
        finally:
            for process in processes:
                if process.poll() is None:
                    process.terminate()
                    process.wait(timeout=3)
            upstream.close()
            logs.close()


if __name__ == "__main__":
    main()
