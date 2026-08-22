#!/usr/bin/env python3
"""Minimal JSON-RPC-over-stdio client for `travsr mcp --stdio`.

MCP is the only external interface travsr has (principle 4: no REST, no GraphQL),
so it is the surface an agent actually uses. The suite was checking the CLI and
not this, which meant the primary interface was the least verified part.

Deliberately hand-rolled rather than pulling an MCP SDK: the rest of this harness
is stdlib-only, and a dependency here would make the suite harder to run than the
thing it tests.
"""

import json
import queue
import subprocess
import threading
import time
from pathlib import Path
from typing import Any, Optional

#: Sentinel pushed when the server closes stdout.
_EOF = object()

PROTOCOL_VERSION = "2024-11-05"

#: Every tool response is wrapped in this before it reaches a model.
ENVELOPE_OPEN = "<travsr-data>"
ENVELOPE_CLOSE = "</travsr-data>"


class McpError(Exception):
    pass


class McpClient:
    """One `travsr mcp --stdio` process, spoken to over line-delimited JSON-RPC."""

    def __init__(self, binary: Path, cwd: Path, env_extra: Optional[dict] = None) -> None:
        import os

        self._proc = subprocess.Popen(
            [str(binary), "mcp", "--stdio"],
            cwd=cwd,
            env={**os.environ, "TRAVSR_DISABLE_REGISTRY": "1", **(env_extra or {})},
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        self._id = 0
        self._stderr: list[str] = []
        #: Notifications (frames with no `id`). The server reports a refusal to
        #: start this way, so they are the failure message when nothing answers.
        self.notifications: list[dict] = []
        self._q: "queue.Queue[object]" = queue.Queue()
        # Both pipes are drained on threads. A chatty server must not be able to
        # fill a pipe and wedge the harness: that is the #503/#572 failure class,
        # and a test harness deadlocking on it would be an unattended stuck job.
        threading.Thread(target=self._read_stderr, daemon=True).start()
        threading.Thread(target=self._read_stdout, daemon=True).start()

    def _read_stderr(self) -> None:
        assert self._proc.stderr is not None
        for line in self._proc.stderr:
            self._stderr.append(line)

    def _read_stdout(self) -> None:
        assert self._proc.stdout is not None
        for line in self._proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                # A log line on stdout is itself a protocol violation, so keep it
                # for the failure message rather than discarding it.
                self._stderr.append(f"[non-JSON on stdout] {line}\n")
                continue
            if "id" in obj:
                self._q.put(obj)
            else:
                self.notifications.append(obj)
        self._q.put(_EOF)

    @property
    def stderr(self) -> str:
        return "".join(self._stderr)

    def _why_it_died(self) -> str:
        """Best available explanation for a server that produced no response."""
        bits = []
        for n in self.notifications:
            msg = (n.get("params") or {}).get("data")
            if msg:
                bits.append(str(msg))
        err = self.stderr.strip()
        if err:
            bits.append(err.splitlines()[-1])
        rc = self._proc.poll()
        if rc is not None:
            bits.append(f"server exited with code {rc}")
        return "; ".join(bits) or "no output and no exit"

    def request(self, method: str, params: Optional[dict] = None, timeout: float = 60.0) -> dict:
        """Send one request and return the parsed response.

        Fails as soon as the server exits rather than waiting out the timeout.
        `travsr mcp --stdio` refuses to start without an index and exits after a
        notification carrying no `id`, so an id-only wait would block for the full
        deadline and report "no response" instead of the reason the server gave.
        """
        assert self._proc.stdin is not None
        self._id += 1
        msg = {"jsonrpc": "2.0", "id": self._id, "method": method, "params": params or {}}
        try:
            self._proc.stdin.write(json.dumps(msg) + "\n")
            self._proc.stdin.flush()
        except (BrokenPipeError, ValueError):
            raise McpError(f"{method}: server is gone ({self._why_it_died()})")

        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise McpError(f"{method}: no response within {timeout}s ({self._why_it_died()})")
            try:
                item = self._q.get(timeout=min(0.5, remaining))
            except queue.Empty:
                if self._proc.poll() is not None:
                    raise McpError(f"{method}: {self._why_it_died()}")
                continue
            if item is _EOF:
                raise McpError(f"{method}: {self._why_it_died()}")
            if isinstance(item, dict) and item.get("id") == self._id:
                return item
            # A response to an earlier request; keep waiting for ours.

    def initialize(self) -> dict:
        r = self.request(
            "initialize",
            {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "travsr-sanity", "version": "1"},
            },
        )
        if "error" in r:
            raise McpError(f"initialize failed: {r['error']}")
        return r["result"]

    def list_tools(self) -> list[dict]:
        r = self.request("tools/list")
        if "error" in r:
            raise McpError(f"tools/list failed: {r['error']}")
        return r["result"].get("tools", [])

    def call_tool(self, name: str, arguments: Optional[dict] = None) -> dict:
        """Raw `tools/call` response, error object included rather than raised.

        Callers need to inspect rejections (a traversal attempt must be refused),
        so this does not raise on a protocol-level error.
        """
        return self.request("tools/call", {"name": name, "arguments": arguments or {}})

    @staticmethod
    def text_of(response: dict) -> str:
        """Concatenated text content of a tools/call result."""
        result = response.get("result") or {}
        parts = []
        for item in result.get("content", []):
            if item.get("type") == "text":
                parts.append(item.get("text", ""))
        return "\n".join(parts)

    def close(self) -> None:
        try:
            if self._proc.stdin:
                self._proc.stdin.close()
            self._proc.wait(timeout=15)
        except Exception:
            self._proc.kill()

    def __enter__(self) -> "McpClient":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()
