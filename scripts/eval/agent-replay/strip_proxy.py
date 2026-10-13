#!/usr/bin/env python3
"""Forward MCP HTTP traffic to an arm and delete `result.structuredContent` from the answers.

    strip_proxy.py --listen 127.0.0.1:8941 --upstream http://127.0.0.1:8841

The text channel of the gate: Claude Code then reads `content[].text`, the channel claude.ai and
ChatGPT read. The proxy changes nothing else. It handles a JSON body and an event stream, because
the streamable HTTP transport answers either way.

The engine declares no `outputSchema`, so a client that validates structured output against a
schema has nothing to complain about once the field is gone. Standard library only.
"""
from __future__ import annotations

import argparse
import http.client
import json
import sys
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HOP = {"connection", "keep-alive", "transfer-encoding", "content-length", "te", "trailers",
       "upgrade", "proxy-authenticate", "proxy-authorization"}


def strip_structured(msg):
    """Delete `result.structuredContent` from a tools/call answer, or from each answer in a batch."""
    if isinstance(msg, list):
        return [strip_structured(m) for m in msg]
    if isinstance(msg, dict):
        res = msg.get("result")
        if isinstance(res, dict) and "content" in res:
            res.pop("structuredContent", None)
    return msg


def _strip_json_text(text: str) -> str:
    try:
        return json.dumps(strip_structured(json.loads(text)), separators=(",", ":"))
    except json.JSONDecodeError:
        return text


def make_server(host, port, upstream):
    up = urllib.parse.urlparse(upstream)

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def _forward(self):
            length = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(length) if length else None
            headers = {k: v for k, v in self.headers.items()
                       if k.lower() not in HOP and k.lower() != "host"}
            conn = http.client.HTTPConnection(up.hostname, up.port, timeout=600)
            conn.request(self.command, self.path, body=body, headers=headers)
            resp = conn.getresponse()
            ctype = resp.getheader("Content-Type", "")
            self.send_response(resp.status)
            for k, v in resp.getheaders():
                if k.lower() not in HOP:
                    self.send_header(k, v)
            if ctype.startswith("text/event-stream"):
                # Streamed line by line: a GET stream stays open, so buffering would hang it.
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True
                for raw in resp:
                    line = raw.decode("utf-8", "replace")
                    if line.startswith("data:"):
                        line = "data: " + _strip_json_text(line[5:].strip()) + "\n"
                    self.wfile.write(line.encode())
                    self.wfile.flush()
            else:
                data = resp.read()
                if ctype.startswith("application/json"):
                    data = _strip_json_text(data.decode("utf-8", "replace")).encode()
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
            conn.close()

        do_GET = do_POST = do_DELETE = do_PUT = do_OPTIONS = _forward

        def log_message(self, *args):
            pass

    return ThreadingHTTPServer((host, port), Handler)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--listen", default="127.0.0.1:8941")
    ap.add_argument("--upstream", required=True)
    a = ap.parse_args(argv)
    host, port = a.listen.rsplit(":", 1)
    make_server(host, int(port), a.upstream).serve_forever()


if __name__ == "__main__":
    sys.exit(main())
