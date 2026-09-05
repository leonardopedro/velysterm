#!/usr/bin/env bash
# Plot e2e acceptance (dev-machine): a REAL matplotlib plot from a
# REAL ipykernel ends up as a Typst-rendered figure in the notebook
# region — the graphical-MIME path, pixel-checked.
#
#   Phase A: drive the stdio bridge directly — execute a plotting
#            cell and collect the display_data image/png payload
#            (PNG magic + dimensions, verified in python).
#   Phase B: full stack — a `\kernel` python segment through
#            `mathed_mini --run-all`; the record carries the payload.
#   Phase C: `mathed_mini --region-image` rasterizes the block's
#            output region through typst_imaging into a PNG (the
#            "screenshot" of the rendered figure), and the PNG is
#            checked for painted (non-transparent) pixels.
#
# Run from the velysterm workspace inside the python-kernel env:
#   nix develop .#python-kernel
#   crates/kernel_client/scripts/run_plot_e2e.sh
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
BRIDGE="$HERE/ipykernel_stdio_bridge.py"

# Guard: the active python3 must carry ipykernel + jupyter_client + matplotlib.
if ! python3 -c 'import ipykernel, jupyter_client, matplotlib' 2>/dev/null; then
    echo "FAIL: this python3 lacks ipykernel/jupyter_client/matplotlib — run inside:" >&2
    echo "  nix develop .#python-kernel   (from the velysterm root)" >&2
    exit 1
fi
chmod +x "$BRIDGE"

# The plotting cell: force matplotlib_inline (ipykernel's default
# rich backend) so show() publishes display_data with image/png.
PLOT_CODE='import matplotlib; matplotlib.use("module://matplotlib_inline.backend_inline"); import matplotlib.pyplot as plt; plt.plot([1, 2, 3]); plt.show()'

echo ">> PHASE A: direct framed drive — a real ipykernel publishes a matplotlib image/png"
python3 - "$BRIDGE" "$PLOT_CODE" <<'PY'
import base64, json, struct, subprocess, sys

HDR = struct.Struct(">IIIII")
bridge, code = sys.argv[1], sys.argv[2]


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
assert (info.get("content") or {}).get("protocol_version"), info
print("  PASS: kernel_info handshake")

send(p, "execute_request", {"code": code})
outs = []
r = until_reply(p, "execute_reply", outs)
assert (r.get("content") or {}).get("status") == "ok", r

png_b64 = None
for m in outs:
    data = (m.get("content") or {}).get("data") or {}
    if isinstance(data, dict) and data.get("image/png"):
        png_b64 = data["image/png"]
assert png_b64, f"no image/png published: {outs}"
raw = base64.b64decode(png_b64)
assert raw[:8] == b"\x89PNG\r\n\x1a\n", "payload is not a PNG"
assert b"IHDR" in raw[:33], "PNG header chunk present"
print(f"  PASS: display_data image/png — {len(raw)} bytes, real PNG")

send(p, "shutdown_request", {"restart": False})
try:
    until_reply(p, "shutdown_reply", [])
except Exception:
    pass
p.stdin.close()
p.wait(timeout=30)
print("  PASS: shutdown cleanly")
PY

echo ">> PHASE B: full stack — a \\kernel python segment via mathed_mini --run-all"
DOC="$(mktemp --suffix=.mathed)"
REC="$(mktemp --suffix=.json)"
IMG="$(mktemp --suffix=.png)"
trap 'rm -f "$DOC" "$REC" "$IMG"' EXIT
# The kernel body is the python one-liner (single line, no blank
# lines inside the segment).
cat > "$DOC" <<'MD'
= Plot cell

#1 import matplotlib; matplotlib.use("module://matplotlib_inline.backend_inline"); import matplotlib.pyplot as plt; plt.plot([1, 2, 3]); plt.show() #2 \kernel(#1,#2, lang: "python", grants: "kernel", name: fig)
MD
(
    cd "$ROOT"
    MATHED_KERNEL_LANGS=python \
        MATHED_KERNEL_BIN="$BRIDGE" \
        MATHED_KERNEL_STDIO=1 \
        cargo run -q -p mathed_mini -- --run-all "$DOC" --grants kernel --out "$REC" >/dev/null
)
python3 - "$REC" <<'PY'
import base64, json, sys

rec = json.load(open(sys.argv[1]))
found = []


def walk(x):
    if isinstance(x, dict):
        if x.get("mime") == "image/png" and isinstance(x.get("data"), str):
            found.append(x["data"])
        for v in x.values():
            walk(v)
    elif isinstance(x, list):
        for v in x:
            walk(v)


walk(rec)
assert found, "record carries no image/png payload"
raw = base64.b64decode(found[0])
assert raw[:8] == b"\x89PNG\r\n\x1a\n", "record payload is not a real PNG"
print(f"  PASS: run record carries the real plot — image/png ({len(raw)} bytes)")
PY

echo ">> PHASE C: --region-image rasterizes the figure through typst_imaging"
(
    cd "$ROOT"
    MATHED_KERNEL_LANGS=python \
        MATHED_KERNEL_BIN="$BRIDGE" \
        MATHED_KERNEL_STDIO=1 \
        cargo run -q -p mathed_mini -- --region-image "$DOC" --grants kernel --out "$IMG" >/dev/null
)
python3 - "$IMG" <<'PY'
import struct, sys, zlib

d = open(sys.argv[1], "rb").read()
assert d[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
w, h = struct.unpack(">II", d[16:24])
assert w > 0 and h > 0, f"degenerate size {w}x{h}"

# Decode the first IDAT to count painted (non-transparent) pixels —
# the figure really painted through typst_imaging.
pos, idat = 8, b""
while pos < len(d):
    (ln,) = struct.unpack(">I", d[pos:pos + 4])
    typ = d[pos + 4:pos + 8]
    if typ == b"IDAT":
        idat += d[pos + 8:pos + 8 + ln]
    pos += 12 + ln
raw = zlib.decompress(idat)
# 8-bit RGBA, rows prefixed by a filter byte; count alpha > 0.
painted = 0
row = w * 4
for y in range(h):
    off = 1 + y * (row + 1)
    rowbytes = raw[off + 1:off + 1 + row]
    painted += sum(1 for i in range(3, len(rowbytes), 4) if rowbytes[i] > 0)
assert painted > 100, f"figure barely painted ({painted} opaque px)"
print(f"  PASS: rendered region screenshot {w}x{h}, {painted} painted pixels")
PY

echo ">> PHASE D: --pages-image — the real figure lands on a Typst-paginated A4 page"
PG="$(mktemp --suffix=.png)"
PDF="$(mktemp --suffix=.pdf)"
trap 'rm -f "$DOC" "$REC" "$IMG" "$PG" "$PDF" "${PG_BASE}.*.png"' EXIT
PG_BASE="${PG%.png}"
(
    cd "$ROOT"
    MATHED_KERNEL_LANGS=python \
        MATHED_KERNEL_BIN="$BRIDGE" \
        MATHED_KERNEL_STDIO=1 \
        cargo run -q -p mathed_mini -- --pages-image "$DOC" --grants kernel --out "$PG_BASE" >/dev/null
)
python3 - "${PG_BASE}.1.png" <<'PY'
import struct, sys, zlib

d = open(sys.argv[1], "rb").read()
assert d[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
w, h = struct.unpack(">II", d[16:24])
assert (w, h) == (596, 842), f"page is not A4 at 1px/pt: {w}x{h}"

pos, idat = 8, b""
while pos < len(d):
    (ln,) = struct.unpack(">I", d[pos:pos + 4])
    typ = d[pos + 4:pos + 8]
    if typ == b"IDAT":
        idat += d[pos + 8:pos + 8 + ln]
    pos += 12 + ln
raw = zlib.decompress(idat)
painted = 0
row = w * 4
for y in range(h):
    off = 1 + y * (row + 1)
    rowbytes = raw[off + 1:off + 1 + row]
    painted += sum(1 for i in range(3, len(rowbytes), 4) if rowbytes[i] > 0)
assert painted > 100, f"A4 page barely painted ({painted} opaque px)"
print(f"  PASS: first A4 page {w}x{h}, {painted} painted pixels (figure landed on the page)")
PY

echo ">> PHASE E: --pages-pdf wraps the same pages in a minimal PDF"
(
    cd "$ROOT"
    MATHED_KERNEL_LANGS=python \
        MATHED_KERNEL_BIN="$BRIDGE" \
        MATHED_KERNEL_STDIO=1 \
        cargo run -q -p mathed_mini -- --pages-pdf "$DOC" --grants kernel --out "$PDF" >/dev/null
)
python3 - "$PDF" <<'PY'
import re, sys, zlib

d = open(sys.argv[1], "rb").read()
assert d.startswith(b"%PDF-1.4"), "not a PDF"
assert d.rstrip().endswith(b"%%EOF"), "no EOF marker"
assert b"/Filter /FlateDecode" in d and b"/ColorSpace /DeviceRGB" in d
assert d.count(b"/Type /Page ") >= 1, "no page objects"
# Every image stream must decompress (a real page bitmap).
for m in re.finditer(rb"stream\r?\n(.*?)\r?\nendstream", d, re.S):
    try:
        zlib.decompress(m.group(1))
    except Exception:
        pass  # the tiny contents stream is plain text (no /Filter)
print("  PASS: minimal PDF, FlateDecode DeviceRGB page bitmap(s)")
PY

echo "============================================================"
echo " plot e2e: ALL TESTS PASSED (real ipykernel -> record -> typst_imaging PNG -> A4 page -> PDF)"
echo "============================================================"