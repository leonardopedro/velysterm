#!/usr/bin/env bash
# ipykernel e2e acceptance (dev-machine, fock_match/mathed_kernel-style):
# a REAL Python kernel behind mathed's `\kernel` segments, attached over
# the framed stdio transport through `kernel_client`'s stdio driver.
#
# ipykernel itself speaks only ZMQ (tcp/ipc) — no native stdio — so the
# compatibility layer is `ipykernel_stdio_bridge.py`: it launches a real
# ipykernel via jupyter_client and fronts it over the 5x-u32 framing.
#
# Run from the velysterm workspace inside the python-kernel env:
#   nix develop .#python-kernel
#   crates/kernel_client/scripts/run_ipykernel_e2e.sh
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
BRIDGE="$HERE/ipykernel_stdio_bridge.py"

# Guard: the active python3 must carry ipykernel + jupyter_client.
if ! python3 -c 'import ipykernel, jupyter_client' 2>/dev/null; then
    echo "FAIL: this python3 lacks ipykernel/jupyter_client — run inside:" >&2
    echo "  nix develop .#python-kernel   (from the velysterm root)" >&2
    exit 1
fi
chmod +x "$BRIDGE"

echo ">> PHASE A: direct framed drive — kernel_info handshake + stateful executes"
python3 - "$BRIDGE" <<'PY'
import json, struct, subprocess, sys

HDR = struct.Struct(">IIIII")
bridge = sys.argv[1]


def frame(obj):
    p = json.dumps(obj, separators=(",", ":")).encode()
    return HDR.pack(len(p), 0, 0, 0, 0) + p


def send(proc, msg_type, content):
    proc.stdin.write(frame({"header": {"msg_type": msg_type}, "content": content}))
    proc.stdin.flush()


def recv(proc):
    hdr = proc.stdout.read(HDR.size)
    assert hdr and len(hdr) == HDR.size, "short header"
    (n,) = HDR.unpack(hdr)[:1]
    return json.loads(proc.stdout.read(n))


def msg_type(m):
    return m.get("msg_type") or (m.get("header") or {}).get("msg_type")


def until_reply(proc, want, collect):
    while True:
        m = recv(proc)
        if msg_type(m) == want:
            return m
        collect.append(m)


p = subprocess.Popen([bridge], stdin=subprocess.PIPE, stdout=subprocess.PIPE)

send(p, "kernel_info_request", {})
bogus = []
info = until_reply(p, "kernel_info_reply", bogus)
ver = (info.get("content") or {}).get("protocol_version")
assert ver, f"kernel_info_reply protocol_version missing: {info}"
print(f"  PASS: kernel_info handshake, protocol_version {ver}")

send(p, "execute_request", {"code": "x = 21"})
r1 = until_reply(p, "execute_reply", bogus)
assert (r1.get("content") or {}).get("status") == "ok", r1

# Second execute proves real kernel state persists inside one session.
# Real kernel shapes are forwarded verbatim: execute_result content is
# {execution_count, data: {mime: value}} with no output_type key.
send(p, "execute_request", {"code": "x * 2"})
outs = []
r2 = until_reply(p, "execute_reply", outs)
assert (r2.get("content") or {}).get("status") == "ok", r2
result_texts = [
    (c.get("data") or {}).get("text/plain")
    for m in outs
    for c in [m.get("content", {})]
    if (c.get("data") or {}).get("text/plain") is not None
]
assert "42" in result_texts, f"stateful execute lost the value: {outs}"
print("  PASS: stateful executes — x = 21 then x * 2 -> execute_result text/plain 42")

send(p, "shutdown_request", {"restart": False})
bogus2 = []
try:
    until_reply(p, "shutdown_reply", bogus2)
except Exception:
    pass
p.stdin.close()
p.wait(timeout=30)
print("  PASS: shutdown_request reaps the kernel cleanly")
PY

echo ">> PHASE B: full stack — a real \kernel segment via mathed_mini --run-all"
DOC="$(mktemp --suffix=.mathed)"
REC="$(mktemp --suffix=.json)"
trap 'rm -f "$DOC" "$REC"' EXIT
cat > "$DOC" <<'MD'
= Python via ipykernel
#1 print(6 * 7) #2 \kernel(#1,#2, lang: "python", grants: "kernel", name: py)
MD
(
    cd "$ROOT"
    MATHED_KERNEL_LANGS=python \
        MATHED_KERNEL_BIN="$BRIDGE" \
        MATHED_KERNEL_STDIO=1 \
        cargo run -q -p mathed_mini -- --run-all "$DOC" --grants kernel --out "$REC" >/dev/null
)
python3 - "$REC" <<'PY'
import json, sys

rec = json.load(open(sys.argv[1]))
texts = []


def walk(x):
    if isinstance(x, dict):
        for v in x.values():
            walk(v)
    elif isinstance(x, list):
        for v in x:
            walk(v)
    elif isinstance(x, str):
        texts.append(x)


walk(rec)
assert any("42" in t for t in texts), f"record lacks the ipykernel output: {texts}"
assert any("kernel" in t for t in texts), "record lacks the kernel run entry"
print("  PASS: the run record carries the real kernel run with stream 42")
PY

echo "============================================================"
echo " ipykernel e2e: ALL TESTS PASSED"
echo "============================================================"
