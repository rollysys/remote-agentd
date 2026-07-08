#!/usr/bin/env python3
"""
test_rust_ssh.py — Full test suite for Rust remote-agentd on J6M board.

Spawns the Rust binary on the board via SSH, runs 21 tests covering
all tools and edge cases. Wire-compatible with the Python prototype tests.
"""

import json
import subprocess
import sys
import os
import time
import re
import argparse

BOARD_HOST = "root@10.64.65.50"
DAEMON_REMOTE_PATH = "/tmp/remote-agentd"


class SSHTunnelClient:
    """Spawns remote-agentd on the board via SSH, communicates via stdio."""

    def __init__(self, host: str, daemon_path: str):
        self.host = host
        self.daemon_path = daemon_path
        self.proc = None
        self._recv_buffer = ""

    def connect(self):
        cmd = [
            "ssh", "-o", "ConnectTimeout=10",
            "-o", "StrictHostKeyChecking=accept-new",
            self.host,
            f"{self.daemon_path}"
        ]
        self.proc = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=0,
        )
        time.sleep(0.3)
        if self.proc.poll() is not None:
            err = self.proc.stderr.read()
            raise RuntimeError(f"Daemon failed to start: {err}")
        self._recv_buffer = ""

    def _send(self, msg: dict) -> None:
        data = json.dumps(msg, ensure_ascii=False, separators=(',', ':'))
        self.proc.stdin.write(data + '\n')
        self.proc.stdin.flush()

    def _recv(self, timeout=30):
        while '\n' not in self._recv_buffer:
            import select
            ready, _, _ = select.select([self.proc.stdout], [], [], timeout)
            if not ready:
                raise TimeoutError(f"Timeout after {timeout}s")
            chunk = os.read(self.proc.stdout.fileno(), 65536)
            if not chunk:
                raise RuntimeError("Daemon closed connection")
            self._recv_buffer += chunk.decode('utf-8', errors='replace')
            timeout = 1
        line, self._recv_buffer = self._recv_buffer.split('\n', 1)
        return json.loads(line.strip())

    def initialize(self) -> dict:
        self._send({
            "jsonrpc": "2.0", "id": "init",
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "rust-test", "version": "0.1.0"}
            }
        })
        while True:
            msg = self._recv(10)
            if msg.get("id") == "init":
                self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})
                return msg["result"]

    _id = 0
    def call_tool(self, name: str, args: dict, collect_progress=False, timeout=60):
        SSHTunnelClient._id += 1
        req_id = f"req_{SSHTunnelClient._id}"
        self._send({
            "jsonrpc": "2.0", "id": req_id,
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": args,
                "_meta": {"progressToken": f"pt_{req_id}"}
            }
        })
        progress = []
        result = None
        t0 = time.time()
        while result is None and time.time() - t0 < timeout:
            msg = self._recv(timeout)
            if msg.get("id") == req_id:
                result = msg
            elif msg.get("method") == "notifications/progress" and collect_progress:
                progress.append(msg.get("params", {}).get("message", ""))
        if result is None:
            raise TimeoutError(f"Tool {name} timed out")
        return result, progress

    def close(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except:
                self.proc.kill()


class TestResults:
    def __init__(self):
        self.passed = 0
        self.failed = 0
        self.errors = []

    def ok(self, name, detail=""):
        self.passed += 1
        print(f"  ✅ {name}" + (f" — {detail}" if detail else ""))

    def fail(self, name, reason=""):
        self.failed += 1
        self.errors.append((name, reason))
        print(f"  ❌ {name}" + (f" — {reason[:120]}" if reason else ""))

    def summary(self):
        total = self.passed + self.failed
        print(f"\n{'═' * 60}")
        print(f"Results: {self.passed}/{total} passed, {self.failed} failed")
        if self.errors:
            for name, reason in self.errors:
                print(f"  • {name}: {reason[:150]}")
        return self.failed == 0


def get_tag(text):
    m = re.search(r'#([0-9A-F]{4})\]', text)
    return m.group(1) if m else None


def run_tests():
    results = TestResults()
    SSHTunnelClient._id = 0
    WORKDIR = "/tmp/rust_agentd_test"

    # ── Setup: create test files on the board ──
    print("\n── Setup: creating test files on board ──")
    setup_cmd = f"""
        mkdir -p {WORKDIR}
        cat > {WORKDIR}/test.cpp << 'HEREDOC'
#include <iostream>

int main() {{
    int x = 42;
    std::cout << "Hello, World!" << std::endl;
    std::cout << "x = " << x << std::endl;

    for (int i = 0; i < 10; i++) {{
        std::cout << i << std::endl;
    }}

    return 0;
}}
HEREDOC

        cat > {WORKDIR}/utils.py << 'HEREDOC'
def foo():
    pass

# TODO: implement bar
def bar():
    pass

# TODO: refactor baz
def baz():
    pass
HEREDOC

        # Large file for streaming test
        python3 -c "
for i in range(1, 201):
    print(f'Line {{i}}: content here')
" > {WORKDIR}/big.txt

        echo "Setup done"
        ls -la {WORKDIR}/
    """
    proc = subprocess.run(
        ["ssh", "-o", "StrictHostKeyChecking=accept-new", BOARD_HOST, setup_cmd],
        capture_output=True, text=True, timeout=30
    )
    if proc.returncode != 0:
        print(f"Setup failed: {proc.stderr}")
        return False
    print(proc.stdout.strip())

    # ── Start daemon ──
    print(f"\n── Starting Rust daemon on {BOARD_HOST} ──")
    client = SSHTunnelClient(BOARD_HOST, DAEMON_REMOTE_PATH)

    try:
        client.connect()
        print("  SSH tunnel established")

        # ── Test 1: Initialize ──
        print("\n── Test 1: Initialize ──")
        try:
            init = client.initialize()
            name = init.get("serverInfo", {}).get("name", "")
            ver = init.get("serverInfo", {}).get("version", "")
            if name == "remote-agentd":
                results.ok(f"Initialize: {name} v{ver}")
            else:
                results.fail("Initialize", f"Unexpected: {init}")
        except Exception as e:
            results.fail("Initialize", str(e))

        # ── Test 2: tools/list ──
        print("\n── Test 2: tools/list ──")
        try:
            client._send({"jsonrpc": "2.0", "id": "tl", "method": "tools/list"})
            msg = client._recv(10)
            tools = msg.get("result", {}).get("tools", [])
            tool_names = [t["name"] for t in tools]
            expected = {"remote_read", "remote_edit", "remote_search",
                       "remote_bash", "remote_find", "remote_write"}
            if expected.issubset(set(tool_names)):
                results.ok(f"tools/list: {len(tools)} tools")
            else:
                results.fail("tools/list", f"Missing: {expected - set(tool_names)}")
        except Exception as e:
            results.fail("tools/list", str(e))

        # ── Test 3: remote_read (file) ──
        print("\n── Test 3: remote_read (file) ──")
        try:
            r, _ = client.call_tool("remote_read", {"path": f"{WORKDIR}/test.cpp"})
            text = r["result"]["content"][0]["text"]
            if "#" in text and "1:#include" in text:
                results.ok("read file with hashline header")
            else:
                results.fail("read file", text[:200])
        except Exception as e:
            results.fail("read file", str(e))

        # ── Test 4: remote_read (line range) ──
        print("\n── Test 4: remote_read (line range 3-5) ──")
        try:
            r, _ = client.call_tool("remote_read", {"path": f"{WORKDIR}/test.cpp:3-5"})
            text = r["result"]["content"][0]["text"]
            if "3:int main()" in text:
                results.ok("read line range 3-5")
            else:
                results.fail("read line range", text[:200])
        except Exception as e:
            results.fail("read line range", str(e))

        # ── Test 5: remote_read (raw) ──
        print("\n── Test 5: remote_read (raw) ──")
        try:
            r, _ = client.call_tool("remote_read", {"path": f"{WORKDIR}/test.cpp:raw"})
            text = r["result"]["content"][0]["text"]
            if "#include" in text and "#[" not in text and "1:" not in text:
                results.ok("read raw mode")
            else:
                results.fail("read raw", text[:200])
        except Exception as e:
            results.fail("read raw", str(e))

        # ── Test 6: remote_read (directory) ──
        print("\n── Test 6: remote_read (directory) ──")
        try:
            r, _ = client.call_tool("remote_read", {"path": WORKDIR})
            text = r["result"]["content"][0]["text"]
            if "test.cpp" in text and "utils.py" in text:
                results.ok("read directory listing")
            else:
                results.fail("read directory", text[:200])
        except Exception as e:
            results.fail("read directory", str(e))

        # ── Test 7: remote_read (streaming, large file) ──
        print("\n── Test 7: remote_read (streaming 200-line file) ──")
        try:
            r, p = client.call_tool("remote_read", {"path": f"{WORKDIR}/big.txt"}, collect_progress=True)
            text = r["result"]["content"][0]["text"]
            if "Line 1" in text or "200" in text:
                results.ok(f"read 200-line file ({len(p)} progress msgs)")
            else:
                results.fail("streaming read", text[:200])
        except Exception as e:
            results.fail("streaming read", str(e))

        # ── Test 8: remote_edit (SWAP) ──
        print("\n── Test 8: remote_edit (SWAP) ──")
        try:
            r, _ = client.call_tool("remote_read", {"path": f"{WORKDIR}/test.cpp"})
            text = r["result"]["content"][0]["text"]
            tag = get_tag(text)

            edit_input = f"[{WORKDIR}/test.cpp#{tag}]\nSWAP 4.=4:\n+    int y = 99;"
            r2, _ = client.call_tool("remote_edit", {"input": edit_input})

            r3, _ = client.call_tool("remote_read", {"path": f"{WORKDIR}/test.cpp:4"})
            text3 = r3["result"]["content"][0]["text"]
            if "int y = 99" in text3:
                results.ok("edit SWAP line 4")
            else:
                results.fail("edit SWAP", f"File not changed. Read back: {text3[:200]}")
        except Exception as e:
            results.fail("edit SWAP", str(e))

        # ── Test 9: remote_edit (DEL) ──
        print("\n── Test 9: remote_edit (DEL) ──")
        try:
            r, _ = client.call_tool("remote_read", {"path": f"{WORKDIR}/test.cpp"})
            text = r["result"]["content"][0]["text"]
            tag = get_tag(text)

            edit_input = f"[{WORKDIR}/test.cpp#{tag}]\nDEL 4"
            client.call_tool("remote_edit", {"input": edit_input})

            r2, _ = client.call_tool("remote_read", {"path": f"{WORKDIR}/test.cpp:4"})
            text2 = r2["result"]["content"][0]["text"]
            if "int y = 99" not in text2:
                results.ok("edit DEL line 4")
            else:
                results.fail("edit DEL", "Line still present")
        except Exception as e:
            results.fail("edit DEL", str(e))

        # ── Test 10: remote_edit (INS.POST) ──
        print("\n── Test 10: remote_edit (INS.POST) ──")
        try:
            r, _ = client.call_tool("remote_read", {"path": f"{WORKDIR}/test.cpp"})
            text = r["result"]["content"][0]["text"]
            tag = get_tag(text)

            edit_input = f"[{WORKDIR}/test.cpp#{tag}]\nINS.POST 3:\n+    // inserted by rust agent"
            client.call_tool("remote_edit", {"input": edit_input})

            r2, _ = client.call_tool("remote_read", {"path": f"{WORKDIR}/test.cpp:3-5"})
            text2 = r2["result"]["content"][0]["text"]
            if "inserted by rust agent" in text2:
                results.ok("edit INS.POST line 3")
            else:
                results.fail("edit INS.POST", text2[:200])
        except Exception as e:
            results.fail("edit INS.POST", str(e))

        # ── Test 11: remote_edit (INS.TAIL) ──
        print("\n── Test 11: remote_edit (INS.TAIL) ──")
        try:
            r, _ = client.call_tool("remote_read", {"path": f"{WORKDIR}/test.cpp"})
            text = r["result"]["content"][0]["text"]
            tag = get_tag(text)

            edit_input = f"[{WORKDIR}/test.cpp#{tag}]\nINS.TAIL:\n+// end of file marker"
            r2, _ = client.call_tool("remote_edit", {"input": edit_input})

            r3, _ = client.call_tool("remote_read", {"path": f"{WORKDIR}/test.cpp"})
            text3 = r3["result"]["content"][0]["text"]
            if "end of file marker" in text3:
                results.ok("edit INS.TAIL")
            else:
                results.fail("edit INS.TAIL", text3[-200:])
        except Exception as e:
            results.fail("edit INS.TAIL", str(e))

        # ── Test 12: remote_search ──
        print("\n── Test 12: remote_search ──")
        try:
            r, _ = client.call_tool("remote_search",
                {"pattern": "TODO", "paths": [WORKDIR]})
            text = r["result"]["content"][0]["text"]
            if "TODO" in text:
                results.ok("search finds TODO")
            else:
                results.fail("search", text[:200])
        except Exception as e:
            results.fail("search", str(e))

        # ── Test 13: remote_search (regex) ──
        print("\n── Test 13: remote_search (regex) ──")
        try:
            r, _ = client.call_tool("remote_search",
                {"pattern": "def \\w+", "paths": [f"{WORKDIR}/utils.py"]})
            text = r["result"]["content"][0]["text"]
            if "def foo" in text or "def bar" in text:
                results.ok("search regex finds function defs")
            else:
                results.fail("search regex", text[:200])
        except Exception as e:
            results.fail("search regex", str(e))

        # ── Test 14: remote_bash ──
        print("\n── Test 14: remote_bash ──")
        try:
            r, _ = client.call_tool("remote_bash",
                {"command": "echo 'hello from J6M' && uname -m"})
            text = r["result"]["content"][0]["text"]
            if "hello from J6M" in text and "aarch64" in text:
                results.ok("bash execution on board")
            else:
                results.fail("bash", text[:200])
        except Exception as e:
            results.fail("bash", str(e))

        # ── Test 15: remote_bash (exit code) ──
        print("\n── Test 15: remote_bash (exit code) ──")
        try:
            r, _ = client.call_tool("remote_bash", {"command": "exit 42"})
            is_error = r["result"].get("isError", False)
            if is_error:
                results.ok("bash non-zero exit code")
            else:
                results.fail("bash exit code", "isError=False")
        except Exception as e:
            results.fail("bash exit code", str(e))

        # ── Test 16: remote_bash (multi-line output) ──
        print("\n── Test 16: remote_bash (multi-line output) ──")
        try:
            r, _ = client.call_tool("remote_bash",
                {"command": "for i in $(seq 1 20); do echo \"line $i\"; done"})
            text = r["result"]["content"][0]["text"]
            lines = text.strip().split('\n')
            if len(lines) >= 20 and "line 1" in text and "line 20" in text:
                results.ok(f"bash 20-line output ({len(lines)} lines)")
            else:
                results.fail("bash multi-line", f"Got {len(lines)} lines")
        except Exception as e:
            results.fail("bash multi-line", str(e))

        # ── Test 17: remote_find ──
        print("\n── Test 17: remote_find ──")
        try:
            r, _ = client.call_tool("remote_find", {"paths": [WORKDIR]})
            text = r["result"]["content"][0]["text"]
            if "test.cpp" in text and "utils.py" in text:
                results.ok("find files on board")
            else:
                results.fail("find", text[:200])
        except Exception as e:
            results.fail("find", str(e))

        # ── Test 18: remote_write + read back ──
        print("\n── Test 18: remote_write + read back ──")
        try:
            r, _ = client.call_tool("remote_write",
                {"path": f"{WORKDIR}/written.txt", "content": "Hello from Rust daemon!\n"})
            r2, _ = client.call_tool("remote_read", {"path": f"{WORKDIR}/written.txt"})
            text = r2["result"]["content"][0]["text"]
            if "Hello from Rust daemon" in text:
                results.ok("write + read back")
            else:
                results.fail("write", text[:200])
        except Exception as e:
            results.fail("write", str(e))

        # ── Test 19: error handling (nonexistent file) ──
        print("\n── Test 19: error handling ──")
        try:
            r, _ = client.call_tool("remote_read", {"path": "/nonexistent/path.txt"})
            if "error" in r or r["result"].get("isError"):
                results.ok("read nonexistent returns error")
            else:
                results.fail("error handling", str(r.get("result", {}))[:200])
        except Exception as e:
            results.fail("error handling", str(e))

        # ── Test 20: stale tag rejection ──
        print("\n── Test 20: stale tag rejection ──")
        try:
            edit_input = f"[{WORKDIR}/test.cpp#0000]\nSWAP 1.=1:\n+// stale"
            r, _ = client.call_tool("remote_edit", {"input": edit_input})
            if r["result"].get("isError") or "error" in r:
                results.ok("stale tag rejected")
            else:
                results.fail("stale tag", str(r.get("result", {}))[:200])
        except Exception as e:
            results.fail("stale tag", str(e))

        # ── Test 21: latency measurement ──
        print("\n── Test 21: latency measurement ──")
        try:
            times = []
            for _ in range(5):
                t0 = time.time()
                client.call_tool("remote_bash", {"command": "echo ok"})
                times.append((time.time() - t0) * 1000)
            avg = sum(times) / len(times)
            results.ok(f"latency: avg={avg:.0f}ms min={min(times):.0f}ms max={max(times):.0f}ms")
        except Exception as e:
            results.fail("latency", str(e))

        # ── Test 22: daemon memory ──
        print("\n── Test 22: daemon memory ──")
        try:
            r, _ = client.call_tool("remote_bash",
                {"command": "ps aux | grep remote-agentd | grep -v grep | awk '{print $6}'"})
            text = r["result"]["content"][0]["text"].strip()
            rss_kb = int(text.split('\n')[0]) if text else 0
            results.ok(f"daemon RSS: {rss_kb / 1024:.1f} MB")
        except Exception as e:
            results.fail("memory", str(e))

        # ── Test 23: multi-op edit (DEL + SWAP in one input) ──
        print("\n── Test 23: multi-op edit ──")
        try:
            r, _ = client.call_tool("remote_read", {"path": f"{WORKDIR}/utils.py"})
            text = r["result"]["content"][0]["text"]
            tag = get_tag(text)

            edit_input = f"[{WORKDIR}/utils.py#{tag}]\nSWAP 1.=1:\n+def foo():\n+    return 42\n[{WORKDIR}/utils.py#{tag}]\nDEL 4"
            r2, _ = client.call_tool("remote_edit", {"input": edit_input})
            if not r2["result"].get("isError"):
                r3, _ = client.call_tool("remote_read", {"path": f"{WORKDIR}/utils.py:1-5"})
                text3 = r3["result"]["content"][0]["text"]
                if "return 42" in text3:
                    results.ok("multi-op edit (SWAP + DEL)")
                else:
                    results.fail("multi-op edit", text3[:200])
            else:
                results.fail("multi-op edit", r2["result"]["content"][0]["text"][:200])
        except Exception as e:
            results.fail("multi-op edit", str(e))

        # ── Test 24: chain edits (read → edit → read → edit) ──
        print("\n── Test 24: chain edits (tag propagation) ──")
        try:
            # First edit
            r, _ = client.call_tool("remote_read", {"path": f"{WORKDIR}/written.txt"})
            text = r["result"]["content"][0]["text"]
            tag1 = get_tag(text)

            edit1 = f"[{WORKDIR}/written.txt#{tag1}]\nINS.TAIL:\n+second line"
            r2, _ = client.call_tool("remote_edit", {"input": edit1})
            text2 = r2["result"]["content"][0]["text"]
            tag2 = get_tag(text2)

            # Second edit using new tag
            edit2 = f"[{WORKDIR}/written.txt#{tag2}]\nINS.TAIL:\n+third line"
            r3, _ = client.call_tool("remote_edit", {"input": edit2})

            r4, _ = client.call_tool("remote_read", {"path": f"{WORKDIR}/written.txt"})
            text4 = r4["result"]["content"][0]["text"]
            if "second line" in text4 and "third line" in text4:
                results.ok(f"chain edits (tag {tag1}→{tag2}→update)")
            else:
                results.fail("chain edits", text4[:200])
        except Exception as e:
            results.fail("chain edits", str(e))

        # ── Test 25: /proc file reading ──
        print("\n── Test 25: read /proc/cpuinfo ──")
        try:
            r, _ = client.call_tool("remote_read", {"path": "/proc/cpuinfo"})
            text = r["result"]["content"][0]["text"]
            if "processor" in text.lower() and "BogoMIPS" in text:
                results.ok("read /proc/cpuinfo")
            else:
                results.fail("/proc/cpuinfo", text[:200])
        except Exception as e:
            results.fail("/proc/cpuinfo", str(e))

        # ── Test 26: search across multiple files ──
        print("\n── Test 26: search across multiple files ──")
        try:
            r, _ = client.call_tool("remote_search",
                {"pattern": "def", "paths": [WORKDIR]})
            text = r["result"]["content"][0]["text"]
            if "utils.py" in text and "def" in text:
                results.ok("multi-file search")
            else:
                results.fail("multi-file search", text[:200])
        except Exception as e:
            results.fail("multi-file search", str(e))

    finally:
        client.close()

    # Cleanup
    subprocess.run(["ssh", BOARD_HOST, f"rm -rf {WORKDIR}"],
                   capture_output=True, timeout=10)

    return results.summary()


if __name__ == "__main__":
    print("═" * 60)
    print("  Rust remote-agentd — Full Test Suite on J6M Board")
    print("═" * 60)

    success = run_tests()
    sys.exit(0 if success else 1)
