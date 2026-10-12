"""The proxy deletes result.structuredContent and changes nothing else."""
import json
import threading
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from strip_proxy import strip_structured, make_server

CALL = {"jsonrpc": "2.0", "id": 3, "result": {
    "content": [{"type": "text", "text": "hello"}], "structuredContent": {"a": 1}, "isError": False}}


def test_structured_content_is_deleted_and_the_text_block_stays():
    out = strip_structured(json.loads(json.dumps(CALL)))
    assert out == {"jsonrpc": "2.0", "id": 3, "result": {
        "content": [{"type": "text", "text": "hello"}], "isError": False}}


def test_a_tools_list_answer_is_untouched():
    msg = {"jsonrpc": "2.0", "id": 2, "result": {"tools": [{"name": "memory_search"}]}}
    assert strip_structured(json.loads(json.dumps(msg))) == msg


def test_a_batch_is_stripped_item_by_item():
    out = strip_structured([json.loads(json.dumps(CALL)), {"jsonrpc": "2.0", "method": "ping"}])
    assert "structuredContent" not in out[0]["result"] and out[1] == {"jsonrpc": "2.0", "method": "ping"}


def _upstream(sse):
    class H(BaseHTTPRequestHandler):
        def do_POST(self):
            self.rfile.read(int(self.headers.get("Content-Length", 0)))
            if sse:
                body = ("event: message\ndata: " + json.dumps(CALL) + "\n\n").encode()
                ctype = "text/event-stream"
            else:
                body, ctype = json.dumps(CALL).encode(), "application/json"
            self.send_response(200)
            self.send_header("Content-Type", ctype)
            self.send_header("Mcp-Session-Id", "abc")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *a):
            pass
    srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv


def _through_proxy(sse):
    up = _upstream(sse)
    px = make_server("127.0.0.1", 0, f"http://127.0.0.1:{up.server_address[1]}")
    threading.Thread(target=px.serve_forever, daemon=True).start()
    req = urllib.request.Request(f"http://127.0.0.1:{px.server_address[1]}/mcp", data=b"{}",
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=5) as r:
        body, hdr = r.read().decode(), r.headers
    up.shutdown(); px.shutdown()
    return body, hdr


def test_a_json_answer_crosses_the_proxy_without_structured_content():
    body, hdr = _through_proxy(sse=False)
    assert "structuredContent" not in body and json.loads(body)["result"]["content"][0]["text"] == "hello"
    assert hdr["Mcp-Session-Id"] == "abc"


def test_an_event_stream_answer_crosses_the_proxy_without_structured_content():
    body, _ = _through_proxy(sse=True)
    data = [l for l in body.splitlines() if l.startswith("data:")]
    assert len(data) == 1 and "structuredContent" not in data[0]
    assert json.loads(data[0][5:])["result"]["content"][0]["text"] == "hello"
