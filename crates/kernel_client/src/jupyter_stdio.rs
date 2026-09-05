//! N11: the Jupyter stdio transport parser (kernel_client side).
//!
//! A Jupyter kernel is a language runtime speaking the Jupyter wire
//! protocol; the `kernel_exec` op is **Jupyter-kernel-compatible** —
//! its outputs mirror the wire protocol's content
//! ([`KernelOutput`]: `stream` / `execute_result` / `error`) and a
//! real kernel over the stdio transport is drivable through the same
//! op. This module implements the stdio framing this project drives
//! and normalizes kernel messages into [`KernelOutput`]s. The
//! generalization over Jupyter stays in the worker's gates: safety
//! comes from **grants, not per-kernel container isolation**.
//!
//! Framing: a message is a 20-byte header — five u32 big-endian
//! fields, the first the JSON payload length in bytes and the other
//! four reserved zeros — followed by that many bytes of JSON message.
//! Decoding stops cleanly at a zero-length payload or end-of-input.

use crate::KernelOutput;

/// Header size: five u32 big-endian fields.
const HEADER_BYTES: usize = 20;

/// Encode one JSON message as a stdio frame (see module docs).
pub fn encode_frame(msg: &serde_json::Value) -> Vec<u8> {
    let payload = msg.to_string().into_bytes();
    let mut out = Vec::with_capacity(HEADER_BYTES + payload.len());
    // Field 0: payload length; fields 1–4: reserved, zero.
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&[0u8; HEADER_BYTES - 4]);
    out.extend_from_slice(&payload);
    out
}

/// Decode a byte stream of stdio frames into JSON messages.
/// `Ok` with the messages when every header + payload parses; a
/// truncated header or payload, or non-JSON payload, is an `Err` —
/// never a silent drop.
pub fn decode_frames(bytes: &[u8]) -> Result<Vec<serde_json::Value>, String> {
    let mut msgs = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        if rest.len() < HEADER_BYTES {
            return Err(format!(
                "truncated frame header: {} bytes left, need {HEADER_BYTES}",
                rest.len()
            ));
        }
        let len = u32::from_be_bytes(rest[0..4].try_into().unwrap()) as usize;
        rest = &rest[HEADER_BYTES..];
        if len == 0 {
            // A zero-length payload ends the exchange cleanly.
            break;
        }
        if rest.len() < len {
            return Err(format!(
                "truncated frame payload: {len} bytes announced, {} available",
                rest.len()
            ));
        }
        let payload = &rest[..len];
        rest = &rest[len..];
        match serde_json::from_slice::<serde_json::Value>(payload) {
            Ok(v) => msgs.push(v),
            Err(e) => return Err(format!("frame payload is not JSON: {e}")),
        }
    }
    Ok(msgs)
}

/// Decode greedily from a live byte stream: every complete frame in
/// `bytes` comes back as a message, and whatever trailing bytes form
/// an incomplete header or payload are returned untouched for the
/// caller to prepend to the next read — the incremental companion to
/// [`decode_frames`], used by the stdio session driver that must not
/// block forever waiting for a frame that is still arriving. A
/// complete frame whose payload is not JSON is still an `Err` (never
/// a silent drop).
pub fn decode_partial(bytes: &[u8]) -> Result<(Vec<serde_json::Value>, Vec<u8>), String> {
    let mut msgs = Vec::new();
    let mut rest = bytes;
    loop {
        if rest.len() < HEADER_BYTES {
            // Too few bytes even for a header: keep the whole tail.
            break;
        }
        let len = u32::from_be_bytes(rest[0..4].try_into().unwrap()) as usize;
        if len == 0 {
            // A zero-length payload ends the exchange (see
            // [`decode_frames`]): drop the marker, keep nothing more.
            rest = &rest[HEADER_BYTES..];
            break;
        }
        if rest.len() < HEADER_BYTES + len {
            // The announced payload has not fully arrived yet.
            break;
        }
        let payload = &rest[HEADER_BYTES..HEADER_BYTES + len];
        match serde_json::from_slice::<serde_json::Value>(payload) {
            Ok(v) => msgs.push(v),
            Err(e) => return Err(format!("frame payload is not JSON: {e}")),
        }
        rest = &rest[HEADER_BYTES + len..];
    }
    Ok((msgs, rest.to_vec()))
}

/// Normalize kernel messages into [`KernelOutput`]s — the mapping a
/// real Jupyter kernel's stream/execute_result/error replies need to
/// feed the `kernel_exec` op's response. Each message is either a
/// bare output object or a Jupyter envelope whose `content` carries
/// the output fields; anything else is skipped (never an error).
pub fn outputs_from_messages(msgs: &[serde_json::Value]) -> Vec<KernelOutput> {
    let mut out = Vec::new();
    for m in msgs {
        let content = m.get("content").unwrap_or(m);
        match content.get("output_type").and_then(|t| t.as_str()) {
            Some("stream") => {
                let name = content
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("stdout")
                    .to_string();
                let text = content
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                if !text.is_empty() {
                    out.push(KernelOutput::Stream { name, text });
                }
            }
            Some("execute_result") | Some("display_data") => {
                let data = content.get("data");
                // MIME-faithful v1: every *string-valued* payload in
                // the data dict survives as its own Result —
                // `text/plain` first (the display order convention),
                // then the rest — so rich payloads (`image/png`,
                // `text/html`, …) are carried into the run record,
                // `ctx.kernel`, and the `.ipynb` projection instead of
                // being dropped. A data dict with no string payload
                // falls back to its JSON dump (never a silent drop).
                let mut emitted = false;
                if let Some(t) = data
                    .and_then(|d| d.get("text/plain"))
                    .and_then(|t| t.as_str())
                {
                    out.push(KernelOutput::Result {
                        mime: "text/plain".to_string(),
                        data: t.to_string(),
                    });
                    emitted = true;
                }
                if let Some(map) = data.and_then(|d| d.as_object()) {
                    for (mime, v) in map {
                        if mime == "text/plain" {
                            continue;
                        }
                        if let Some(s) = v.as_str() {
                            out.push(KernelOutput::Result {
                                mime: mime.clone(),
                                data: s.to_string(),
                            });
                            emitted = true;
                        }
                    }
                }
                if !emitted {
                    let text = data.map(|d| d.to_string()).unwrap_or_default();
                    if !text.is_empty() {
                        out.push(KernelOutput::Result {
                            mime: "text/plain".to_string(),
                            data: text,
                        });
                    }
                }
            }
            Some("error") => out.push(KernelOutput::Error {
                ename: content
                    .get("ename")
                    .and_then(|e| e.as_str())
                    .unwrap_or("KernelFailed")
                    .to_string(),
                evalue: content
                    .get("evalue")
                    .and_then(|e| e.as_str())
                    .unwrap_or("")
                    .to_string(),
                traceback: content
                    .get("traceback")
                    .and_then(|t| t.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|l| l.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_frames_round_trip_a_canned_kernel_exchange() {
        // The canned bytes of a kernel exchange: a stream message, an
        // execute_result, and an error — encoded, decoded, and
        // normalized back into KernelOutputs (the `kernel_exec` op's
        // response shape).
        let msgs = vec![
            serde_json::json!({"msg_type": "stream", "content": {"output_type": "stream", "name": "stdout", "text": "hi\n"}}),
            serde_json::json!({"msg_type": "execute_reply", "content": {"output_type": "execute_result", "data": {"text/plain": "= 0.5"}}}),
            serde_json::json!({"msg_type": "error", "content": {"output_type": "error", "ename": "ZeroDivisionError", "evalue": "boom", "traceback": ["Traceback"]}}),
        ];
        let mut wire = Vec::new();
        for m in &msgs {
            wire.extend_from_slice(&encode_frame(m));
        }
        let decoded = decode_frames(&wire).expect("frames decode");
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0], msgs[0], "payload round-trips exactly");

        let outputs = outputs_from_messages(&decoded);
        assert_eq!(
            outputs,
            vec![
                KernelOutput::Stream {
                    name: "stdout".to_string(),
                    text: "hi\n".to_string(),
                },
                KernelOutput::Result {
                    mime: "text/plain".to_string(),
                    data: "= 0.5".to_string(),
                },
                KernelOutput::Error {
                    ename: "ZeroDivisionError".to_string(),
                    evalue: "boom".to_string(),
                    traceback: vec!["Traceback".to_string()],
                },
            ],
            "wire content normalized to the op's output contract"
        );
    }

    #[test]
    fn multi_mime_data_dicts_keep_every_string_payload() {
        // A real kernel's display_data carries a *dict* of mimes: the
        // normalization must keep text/plain first and then every
        // other string-valued payload (image/png base64, text/html),
        // so rich outputs survive to the record / ctx / .ipynb.
        let msgs = vec![
            serde_json::json!({ "msg_type": "display_data", "content": {
                "output_type": "display_data",
                "data": {
                    "text/plain": "a plot",
                    "image/png": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB",
                    "text/html": "<svg></svg>",
                },
            }}),
            serde_json::json!({ "msg_type": "execute_result", "content": {
                "output_type": "execute_result",
                "data": { "text/html": "<b>hi</b>" },
            }}),
        ];
        let outputs = outputs_from_messages(&msgs);
        assert_eq!(
            outputs,
            vec![
                KernelOutput::Result {
                    mime: "text/plain".to_string(),
                    data: "a plot".to_string(),
                },
                KernelOutput::Result {
                    mime: "image/png".to_string(),
                    data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB".to_string(),
                },
                KernelOutput::Result {
                    mime: "text/html".to_string(),
                    data: "<svg></svg>".to_string(),
                },
                KernelOutput::Result {
                    mime: "text/html".to_string(),
                    data: "<b>hi</b>".to_string(),
                },
            ],
            "every string payload survives, text/plain first"
        );
    }

    #[test]
    fn data_dict_without_string_payloads_falls_back_not_drops() {
        // Non-string mime values (nested JSON) have no v1 string
        // home: the whole dict is kept as a JSON text/plain dump so
        // nothing is silently dropped.
        let msgs = vec![
            serde_json::json!({ "msg_type": "execute_result", "content": {
                "output_type": "execute_result",
                "data": { "application/json": { "a": [1, 2] } },
            }}),
        ];
        let outputs = outputs_from_messages(&msgs);
        assert_eq!(outputs.len(), 1);
        match &outputs[0] {
            KernelOutput::Result { mime, data } => {
                assert_eq!(mime, "text/plain");
                assert!(data.contains("application/json"), "dump kept: {data}");
            }
            o => panic!("expected a Result dump, got {o:?}"),
        }
    }

    #[test]
    fn decode_partial_keeps_incomplete_tail_for_the_next_read() {
        // Two frames, fed to decode_partial a few bytes at a time:
        // complete frames surface immediately and the partial tail is
        // returned for the caller to prepend — the incremental
        // contract the stdio session driver relies on.
        let msgs = vec![
            serde_json::json!({"msg_type": "stream", "content": {"output_type": "stream", "name": "stdout", "text": "a\n"}}),
            serde_json::json!({"msg_type": "execute_reply", "content": {"status": "ok"}}),
        ];
        let mut wire = Vec::new();
        for m in &msgs {
            wire.extend_from_slice(&encode_frame(m));
        }
        let mut pending = Vec::new();
        let mut decoded = Vec::new();
        for chunk in wire.chunks(7) {
            pending.extend_from_slice(chunk);
            let (got, rest) = decode_partial(&pending).expect("partial decode");
            decoded.extend(got);
            pending = rest;
        }
        assert!(pending.is_empty(), "no dangling tail after the full feed");
        assert_eq!(decoded.len(), 2, "both frames decoded exactly once");
        assert_eq!(decoded[0], msgs[0]);
        assert_eq!(decoded[1], msgs[1]);
    }

    #[test]
    fn decode_rejects_truncated_or_garbage_frames() {
        let msg = serde_json::json!({"a": 1});
        let frame = encode_frame(&msg);
        // Truncated payload: announce more bytes than exist.
        let mut bad = frame.clone();
        bad[0] = 0xFF;
        assert!(decode_frames(&bad).is_err(), "oversized length refused");
        // Truncated header.
        assert!(decode_frames(&frame[..10]).is_err(), "short header refused");
        // Garbage payload.
        let mut garbage = frame[..HEADER_BYTES].to_vec();
        garbage.extend_from_slice(b"not json at all");
        assert!(decode_frames(&garbage).is_err(), "non-JSON payload refused");
    }
}
