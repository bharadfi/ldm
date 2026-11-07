import sys, json, time, re
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler

LOG = sys.argv[1]
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8765
MB = 1 << 20
FILES = {  # path -> (size, ranges, extra headers, throttle bytes/sec)
    "/file.bin": (40 * MB, True, {}, 0),
    "/norange.bin": (10 * MB, False, {}, 0),
    "/cd": (5 * MB, True, {"Content-Disposition": 'attachment; filename="report final.bin"'}, 0),
    "/slow.bin": (30 * MB, True, {}, 3 * MB),
    "/vslow.bin": (30 * MB, True, {}, 150 * 1024),
}
PAT = bytes(i % 251 for i in range(251 * 4096))
LM = "Wed, 01 Jan 2025 00:00:00 GMT"


def chunk(off, n):
    s = off % 251
    out = (PAT[s:] + PAT)[:n] if n <= len(PAT) - 251 else None
    return out


def log(rec):
    with open(LOG, "a") as f:
        f.write(json.dumps(rec) + "\n")


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def handle_one(self, method):
        p = self.path.split("?")[0]
        rec = {"t": round(time.time(), 3), "client": self.client_address[1], "method": method,
               "path": self.path, "headers": dict(self.headers)}
        if p == "/redir.bin":
            rec["status"] = 302
            log(rec)
            self.send_response(302)
            self.send_header("Location", "/file.bin")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if p not in FILES:
            rec["status"] = 404
            log(rec)
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        size, ranges, extra, thr = FILES[p]
        start, end, status = 0, size - 1, 200
        rng = self.headers.get("Range")
        ifr = self.headers.get("If-Range")
        if rng and ranges and (not ifr or ifr in ('"v1"', LM)):
            m = re.match(r"bytes=(\d*)-(\d*)$", rng.strip())
            if m:
                a, b = m.groups()
                if a == "":
                    start = max(0, size - int(b))
                else:
                    start = int(a)
                    if b:
                        end = min(int(b), size - 1)
                if start >= size:
                    rec["status"] = 416
                    log(rec)
                    self.send_response(416)
                    self.send_header("Content-Range", f"bytes */{size}")
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                status = 206
        length = end - start + 1
        rec.update(status=status, served=[start, end])
        log(rec)
        self.send_response(status)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(length))
        self.send_header("ETag", '"v1"')
        self.send_header("Last-Modified", LM)
        if ranges:
            self.send_header("Accept-Ranges", "bytes")
        if status == 206:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        for k, v in extra.items():
            self.send_header(k, v)
        self.end_headers()
        if method == "HEAD":
            return
        sent, step = 0, 64 * 1024
        try:
            while sent < length:
                n = min(step, length - sent)
                s = (start + sent) % 251
                self.wfile.write(PAT[s:s + n])
                sent += n
                if thr:
                    time.sleep(n / thr)
        except (ConnectionError, OSError):
            log({"t": round(time.time(), 3), "client": self.client_address[1], "event": "aborted", "sent": sent, "of": length})

    def do_GET(self):
        self.handle_one("GET")

    def do_HEAD(self):
        self.handle_one("HEAD")


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
