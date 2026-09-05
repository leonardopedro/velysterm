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
                let text = data
                    .and_then(|d| d.get("text/plain"))
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| data.map(|d| d.to_string()).unwrap_or_default());
                out.push(KernelOutput::Result {
                    mime: "text/plain".to_string(),
                    data: text,
                });
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
