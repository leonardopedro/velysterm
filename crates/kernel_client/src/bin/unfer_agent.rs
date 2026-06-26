//! unfer_agent — NDJSON request/response loop on stdin/stdout.
//!
//! Each line on stdin is a JSON object:
//! ```json
//! {"id":"1","op":"version","params":{}}
//! ```
//! Each response is a single JSON object on stdout:
//! ```json
//! {"id":"1","ok":true,"result":{"version":"0.1.0"}}
//! ```
//!
//! Ops:
//! - `version` — kernel version string.
//! - `create_model` — create a Session from a `ModelSpec`; returns a model id.
//! - `set_prior` — replace the prior state of a model.
//! - `evolve` — time-evolve a model by `t`.
//! - `condition` — condition on an event predicate; returns prior P(e).
//! - `probability` — query P(event) for a model.
//! - `snapshot` — return top-k state components.
//! - `list_codes` — dump all UK-#### codes for self-documentation.
//!
//! Unknown ops return `ok:false` with code UK-1001 and a `ReplaceValue`
//! hint listing the valid op names.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};

use prob_kernel::Session;
use unfer_protocol::{
    AgentRequest, AgentResponse, Code, Diagnostic, EventPredicate,
    HintKind, ModelSpec, PriorSpec, RepairHint, Severity, codes,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const VALID_OPS: &[&str] = &[
    "version",
    "create_model",
    "set_prior",
    "evolve",
    "condition",
    "probability",
    "snapshot",
    "list_codes",
];

fn unknown_op_diag(op: &str) -> Diagnostic {
    Diagnostic::new(
        Code::BAD_JSON,
        format!("Unknown op '{}'", op),
        Severity::Error,
    )
    .with_hint(RepairHint::new(
        HintKind::ReplaceValue,
        "op",
        format!("One of: {}", VALID_OPS.join(", ")),
    ))
}

fn bad_json_diag(msg: &str) -> Diagnostic {
    Diagnostic::new(
        Code::BAD_JSON,
        format!("Invalid JSON: {}", msg),
        Severity::Error,
    )
}

struct AgentState {
    sessions: HashMap<u64, Session>,
    next_id: u64,
}

impl AgentState {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            next_id: 1,
        }
    }

    fn handle(&mut self, req: &AgentRequest) -> AgentResponse {
        match req.op.as_str() {
            "version" => AgentResponse::ok(
                &req.id,
                serde_json::json!({ "version": VERSION }),
            ),
            "list_codes" => {
                let codes: Vec<serde_json::Value> = codes::all()
                    .iter()
                    .map(|(code, name, desc)| {
                        serde_json::json!({
                            "code": code,
                            "name": name,
                            "description": desc,
                        })
                    })
                    .collect();
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({ "codes": codes }),
                )
            }
            "create_model" => {
                let spec: ModelSpec = match serde_json::from_value(
                    req.params.clone(),
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag(&e.to_string()),
                        );
                    }
                };
                match Session::new(&spec) {
                    Ok(session) => {
                        let id = self.next_id;
                        self.next_id += 1;
                        self.sessions.insert(id, session);
                        AgentResponse::ok(
                            &req.id,
                            serde_json::json!({ "model_id": id }),
                        )
                    }
                    Err(e) => {
                        AgentResponse::err(&req.id, e.to_diagnostic())
                    }
                }
            }
            "set_prior" => {
                let (model_id, prior) = match parse_model_and_param::<
                    PriorSpec,
                >(
                    &req.params, "prior"
                ) {
                    Ok(v) => v,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                match self.sessions.get_mut(&model_id) {
                    Some(session) => {
                        match session.set_prior(&prior) {
                            Ok(_) => AgentResponse::ok(
                                &req.id,
                                serde_json::json!({ "ok": true }),
                            ),
                            Err(e) => AgentResponse::err(
                                &req.id,
                                e.to_diagnostic(),
                            ),
                        }
                    }
                    None => AgentResponse::err(
                        &req.id,
                        bad_handle_diag(model_id),
                    ),
                }
            }
            "evolve" => {
                let (model_id, t) = match parse_model_and_param::<f64>(
                    &req.params,
                    "t",
                ) {
                    Ok(v) => v,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                match self.sessions.get_mut(&model_id) {
                    Some(session) => match session.evolve(t) {
                        Ok(report) => AgentResponse::ok(
                            &req.id,
                            serde_json::to_value(report).unwrap(),
                        ),
                        Err(e) => AgentResponse::err(
                            &req.id,
                            e.to_diagnostic(),
                        ),
                    },
                    None => AgentResponse::err(
                        &req.id,
                        bad_handle_diag(model_id),
                    ),
                }
            }
            "probability" => {
                let (model_id, event) = match parse_model_and_param::<
                    EventPredicate,
                >(
                    &req.params, "event"
                ) {
                    Ok(v) => v,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                match self.sessions.get(&model_id) {
                    Some(session) => {
                        match session.probability(&event) {
                            Ok(p) => AgentResponse::ok(
                                &req.id,
                                serde_json::json!({ "probability": p }),
                            ),
                            Err(e) => AgentResponse::err(
                                &req.id,
                                e.to_diagnostic(),
                            ),
                        }
                    }
                    None => AgentResponse::err(
                        &req.id,
                        bad_handle_diag(model_id),
                    ),
                }
            }
            "condition" => {
                let (model_id, event) = match parse_model_and_param::<
                    EventPredicate,
                >(
                    &req.params, "event"
                ) {
                    Ok(v) => v,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                match self.sessions.get_mut(&model_id) {
                    Some(session) => {
                        match session.condition(&event) {
                            Ok(p) => AgentResponse::ok(
                                &req.id,
                                serde_json::json!({ "prior_probability": p }),
                            ),
                            Err(e) => AgentResponse::err(
                                &req.id,
                                e.to_diagnostic(),
                            ),
                        }
                    }
                    None => AgentResponse::err(
                        &req.id,
                        bad_handle_diag(model_id),
                    ),
                }
            }
            "snapshot" => {
                let (model_id, top_k) = match parse_model_and_param::<
                    usize,
                >(
                    &req.params, "top_k"
                ) {
                    Ok(v) => v,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                match self.sessions.get(&model_id) {
                    Some(session) => {
                        let summary = session.snapshot(top_k);
                        AgentResponse::ok(
                            &req.id,
                            serde_json::to_value(summary).unwrap(),
                        )
                    }
                    None => AgentResponse::err(
                        &req.id,
                        bad_handle_diag(model_id),
                    ),
                }
            }
            _ => {
                AgentResponse::err(&req.id, unknown_op_diag(&req.op))
            }
        }
    }
}

fn bad_handle_diag(model_id: u64) -> Diagnostic {
    Diagnostic::new(
        Code::BAD_HANDLE,
        format!("No model with id {}", model_id),
        Severity::Error,
    )
}

fn parse_model_and_param<T: serde::de::DeserializeOwned>(
    params: &serde_json::Value,
    param_name: &str,
) -> Result<(u64, T), Diagnostic> {
    let model_id = params
        .get("model_id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| {
            bad_json_diag("missing or non-integer 'model_id' field")
        })?;
    let param = params.get(param_name).ok_or_else(|| {
        bad_json_diag(&format!("missing '{}' field", param_name))
    })?;
    let value: T =
        serde_json::from_value(param.clone()).map_err(|e| {
            bad_json_diag(&format!("invalid '{}': {}", param_name, e))
        })?;
    Ok((model_id, value))
}

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let mut state = AgentState::new();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: AgentRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let resp = AgentResponse::err(
                    "unknown",
                    bad_json_diag(&e.to_string()),
                );
                let _ = writeln!(
                    stdout,
                    "{}",
                    serde_json::to_string(&resp).unwrap()
                );
                let _ = stdout.flush();
                continue;
            }
        };
        let resp = state.handle(&req);
        let _ = writeln!(
            stdout,
            "{}",
            serde_json::to_string(&resp).unwrap()
        );
        let _ = stdout.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_op() {
        let mut state = AgentState::new();
        let req =
            AgentRequest::new("1", "version", serde_json::json!({}));
        let resp = state.handle(&req);
        assert!(resp.ok);
        assert_eq!(resp.id, "1");
    }

    #[test]
    fn list_codes_op() {
        let mut state = AgentState::new();
        let req = AgentRequest::new(
            "2",
            "list_codes",
            serde_json::json!({}),
        );
        let resp = state.handle(&req);
        assert!(resp.ok);
    }

    #[test]
    fn unknown_op_returns_hint() {
        let mut state = AgentState::new();
        let req = AgentRequest::new(
            "3",
            "frobnicate",
            serde_json::json!({}),
        );
        let resp = state.handle(&req);
        assert!(!resp.ok);
        let diag = resp.error.unwrap();
        assert_eq!(diag.code, Code::BAD_JSON);
        assert!(!diag.hints.is_empty());
        let hint = &diag.hints[0];
        assert!(hint.suggestion.contains("version"));
    }

    #[test]
    fn bad_model_handle() {
        let mut state = AgentState::new();
        let req = AgentRequest::new(
            "4",
            "evolve",
            serde_json::json!({"model_id": 999, "t": 1.0}),
        );
        let resp = state.handle(&req);
        assert!(!resp.ok);
        let diag = resp.error.unwrap();
        assert_eq!(diag.code, Code::BAD_HANDLE);
    }
}
