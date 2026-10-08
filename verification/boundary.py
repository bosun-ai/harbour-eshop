"""Disposable black-box characterization; never use production accounts or state."""
import argparse
import http.client
import http.cookies
import re
import socket
import ssl
import urllib.parse


class Client:
    def __init__(self, port, ca):
        self.port = port
        self.context = ssl.create_default_context(cafile=ca)
        self.cookies = {}
        self.tokens = {}

    def request(self, path, method="GET", data=None):
        body = urllib.parse.urlencode(data).encode() if data is not None else None
        headers = {"Host": "localhost:8002"}
        if body is not None:
            headers["Content-Type"] = "application/x-www-form-urlencoded"
        if self.cookies:
            headers["Cookie"] = "; ".join(f"{key}={value}" for key, value in self.cookies.items())
        connection = http.client.HTTPSConnection("localhost", self.port, context=self.context, timeout=8)
        try:
            connection.request(method, path, body, headers)
            response = connection.getresponse()
            content = response.read()
            cookies = response.headers.get_all("Set-Cookie", [])
            normalized = []
            for value in cookies:
                parsed = http.cookies.SimpleCookie(value)
                for key, morsel in parsed.items():
                    token = morsel.value
                    if token:
                        self.tokens.setdefault(token, f"session-{len(self.tokens)}")
                    self.cookies[key] = token
                    value = value.replace(token, self.tokens[token]) if token else value
                normalized.append(value)
            return response.status, response.getheader("Location"), response.getheader("Content-Type"), normalized, content
        finally:
            connection.close()


def scenario(client):
    results = []

    def call(path, method="GET", data=None, status=None):
        result = client.request(path, method, data)
        if status is not None:
            assert result[0] == status, (path, result[0], status)
        results.append((path, result))
        return result

    assert call("/hello", status=200)[4] == b"Hello!"
    assert call("/", status=303)[1] == "/app/login"
    for path in ["/unknown", "/files/missing.css", "/ready", "/live"]:
        call(path, status=404)
    call("/hello", "HEAD", status=501)
    call("/hello", "PUT", status=501)
    call("/files/main.css", status=200)
    call("/app/login", "POST", {"user": "missing", "password": "bad"}, 303)
    call("/app/login?err", status=200)
    call("/app/register", status=200)
    call("/app/register", "POST", {"user": "boundary", "name": "Retained", "password1": "x", "password2": "y"}, 303)
    assert b'Retained' in call("/app/register?err=2", status=200)[4]
    call("/app/register", "POST", {"user": "boundary", "name": "Boundary", "password1": "secret", "password2": "secret"}, 303)
    for path in ["/app/main", "/app/account", "/app/shopping", "/app/shopping?_pos=11", "/app/cart"]:
        call(path, status=200)
    call("/app/account/edit", "POST", {"name": "Retried", "password1": "x", "password2": "y"}, 303)
    assert b"Retried" in call("/app/account/edit?err=2", status=200)[4]
    call("/app/account/edit", "POST", {"name": "Updated"}, 303)
    call("/app/account", status=200)
    call("/app/shopping?add=0001", status=303)
    call("/app/shopping?add=0001", status=303)
    assert b"53.34" in call("/app/cart", status=200)[4]
    call("/app/cart?del=0001", status=303)
    call("/app/cart", status=200)  # Characterization, not a deletion promise.
    call("/app/logout", status=200)
    call("/app/main", status=303)
    call("/app/login", "POST", {"user": "boundary", "password": "secret"}, 303)
    call("/app/main", status=200)
    for path in ["/hello?raw=%2F%26+%25&raw=two", "/%68ello", "/files/%6dain.css"]:
        call(path)
    return results


def raw(port, ca, request):
    context = ssl.create_default_context(cafile=ca)
    with socket.create_connection(("localhost", port), timeout=8) as tcp:
        with context.wrap_socket(tcp, server_hostname="localhost") as tls:
            tls.sendall(request)
            result = b""
            while True:
                chunk = tls.recv(65536)
                if not chunk:
                    return result
                result += chunk


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=["compare", "rollback", "failure", "restored", "transport"])
    parser.add_argument("--ca", required=True)
    parser.add_argument("--direct", type=int, default=18002)
    parser.add_argument("--proxy", type=int, default=8002)
    args = parser.parse_args()
    proxy = Client(args.proxy, args.ca)
    if args.mode == "compare":
        direct_results = scenario(Client(args.direct, args.ca))
        proxy_results = scenario(proxy)
        assert direct_results == proxy_results, next(((left, right) for left, right in zip(direct_results, proxy_results) if left != right), "length mismatch")
        print(f"PASS: {len(proxy_results)} direct/proxy hops; opaque session tokens mapped")
    elif args.mode == "rollback":
        assert proxy.request("/hello")[4] == b"Hello!"
        assert proxy.request("/app/main")[0] == 303
        assert proxy.request("/app/login", "POST", {"user": "boundary", "password": "secret"})[0] == 303
        assert b"Updated" in proxy.request("/app/account")[4]
        assert b"53.34" in proxy.request("/app/cart")[4]
        assert proxy.request("/app/shopping")[0] == 200
        print("PASS: rollback retains account, catalogue, cart; restart loses sessions")
    elif args.mode in ("failure", "restored"):
        assert proxy.request("/hello")[0] == (502 if args.mode == "failure" else 200)
        print(f"PASS: upstream {args.mode}")
    else:
        for headers, expected in [(b"Transfer-Encoding: chunked\r\n", 411), (b"Content-Length: 5\r\nExpect: 100-continue\r\n", 417)]:
            result = raw(args.proxy, args.ca, b"POST /app/login HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n" + headers + b"\r\n")
            assert re.search(rb"HTTP/1.1 " + str(expected).encode(), result), result
        result = raw(args.proxy, args.ca, b"GET /info HTTP/1.1\r\nHost: preserved.example\r\nCookie: a=1\r\nCookie: b=2\r\nConnection: close, X-Remove\r\nX-Remove: secret-marker\r\nX-Forwarded-For: spoof-marker\r\n\r\n")
        assert b"preserved.example" in result and b"spoof-marker" not in result and b"secret-marker" not in result
        print("PASS: chunked=411, Expect=417; Host retained, spoof/hop headers removed")


if __name__ == "__main__":
    main()
