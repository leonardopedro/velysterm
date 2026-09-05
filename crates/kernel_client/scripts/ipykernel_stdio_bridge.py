#!/usr/bin/env python3
"""ipykernel stdio bridge — a REAL Jupyter kernel behind mathed `\\kernel`.

ipykernel itself only speaks ZMQ (tcp/ipc) — there is no native stdio
transport, so this adapter is the compatibility layer: it launches a
real ipykernel through jupyter_client and fronts it over the framed
stdio transport `kernel_client`'s stdio driver speaks (5 x u32
big-endian header, first word = JSON payload length, four reserved
zeros). The grant + language gates stay in the Rust worker; this
process is the kernel it points `MATHED_KERNEL_BIN` at with
`MATHED_KERNEL_STDIO` set.

Exchange handled:
  kernel_info_request  -> kernel_info_reply (handshake)
  execute_request      -> the kernel's iopub outputs (stream /
                          execute_result / display_data / error) then
                          its execute_reply — in wire order, so the
                          driver's stop-at-reply normalization sees
                          every output
  shutdown_request     -> shutdown_reply, then the kernel is reaped

State persists across execute_requests within one session (a real
kernel property); mathed's one-shot `\\kernel` segment semantics come
from the driver launching a fresh bridge per segment.

Requires: python3 with ipykernel + jupyter_client
(nix develop .#python-kernel in the velysterm workspace).
"""

import json
import struct
import sys

from jupyter_client.manager import KernelManager

HDR = struct.Struct(">IIIII")
KERNEL_NAME = "python3"


def read_frame():
    hdr = sys.stdin.buffer.read(HDR.size)
    if not hdr or len(hdr) < HDR.size:
        return None
    (n,) = HDR.unpack(hdr)[:1]
    if n == 0:
        return None
    body = sys.stdin.buffer.read(n)
    if len(body) < n:
        return None
    try:
        return json.loads(body.decode("utf-8"))
    except ValueError:
        return None


def send(obj):
    payload = json.dumps(obj, separators=(",", ":")).encode("utf-8")
    sys.stdout.buffer.write(HDR.pack(len(payload), 0, 0, 0, 0))
    sys.stdout.buffer.write(payload)
    sys.stdout.buffer.flush()


def send_msg(msg_type, content):
    send({"msg_type": msg_type, "content": content})


def shell_reply(kc, result):
    """jupyter_client's send methods return either the reply dict
    (older blocking clients) or the request msg_id (>= 8): normalize
    both to the shell reply dict."""
    if isinstance(result, str):
        mid = result
        while True:
            m = kc.get_shell_msg(timeout=60)
            if m.get("parent_header", {}).get("msg_id") == mid:
                return m
    return result


def main():
    km = KernelManager(kernel_name=KERNEL_NAME)
    km.start_kernel()
    kc = km.blocking_client()
    kc.start_channels()
    kc.wait_for_ready(timeout=60)

    try:
        while True:
            req = read_frame()
            if req is None:
                break
            mt = req.get("header", {}).get("msg_type") or req.get("msg_type")
            if mt == "kernel_info_request":
                reply = shell_reply(kc, kc.kernel_info())
                send_msg("kernel_info_reply", reply.get("content", {}))
            elif mt == "execute_request":
                code = req.get("content", {}).get("code", "")
                # Blocking execute returns after the shell reply;
                # the iopub outputs for this execution are queued
                # ahead of its idle status.
                reply = shell_reply(kc, kc.execute(code))
                msg_id = reply.get("parent_header", {}).get("msg_id")
                outputs = []
                while True:
                    try:
                        m = kc.get_iopub_msg(timeout=30)
                    except Exception:
                        break
                    if m.get("parent_header", {}).get("msg_id") != msg_id:
                        continue
                    t = m.get("header", {}).get("msg_type")
                    c = m.get("content", {})
                    if t == "status" and c.get("execution_state") == "idle":
                        break
                    if t in ("stream", "execute_result", "display_data", "error"):
                        outputs.append((t, c))
                for t, c in outputs:
                    send_msg(t, c)
                send_msg("execute_reply", reply.get("content", {}))
            elif mt == "shutdown_request":
                try:
                    km.shutdown_kernel(now=True)
                except Exception:
                    pass
                send_msg("shutdown_reply", {"restart": False})
                break
    finally:
        try:
            kc.stop_channels()
        except Exception:
            pass
        try:
            km.shutdown_kernel(now=True)
        except Exception:
            pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
