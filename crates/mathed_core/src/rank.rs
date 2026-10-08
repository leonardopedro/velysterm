//! D-C4 / D10 — advisory ranking for the editor's list surfaces.
//!
//! # What this is
//!
//! An *ordering*, nothing more. [`rank_rows`] returns a permutation of
//! `0..n`; the set of items a caller renders is unchanged by construction,
//! because it never sees a different set — only a different sequence of the
//! same indices. A wrong guess costs a scroll, never a wrong citation.
//!
//! # Where the engine runs: locally (the D10 design decision)
//!
//! The plan left one question open — *"Does D-C4's ranking run locally in
//! velysterm against a small engine state, or against the unfer service?"*
//! The answer recorded on 2026-10-08 is **locally**, for three reasons:
//!
//! 1. **The embedding source is needed either way.** The unfer service
//!    *receives* vectors (`decide_update`); it does not produce them. So
//!    going through the service would not remove the editor's need for
//!    features — it would add a round trip to an ordering that is drawn on
//!    every caret move, and buy no correctness.
//! 2. **The service path is not expressible on D5's wire.** The committed
//!    observe set is `decide_update` / `decide_answer` / `decide_state` /
//!    `decide_recalibrate`. `decide_answer`'s `Choice` scores are
//!    max-over-classes, which [`decide_core::consumers::ranked`] itself
//!    documents as near-constant across inputs, so it cannot produce a
//!    focus-aware order. Doing it service-side would mean a fifth,
//!    rank-shaped op — a protocol change for a UI convenience.
//! 3. **Neither path is in a trust path.** Ranking output is `observe`-kind
//!    advice that cannot gate, sign or certify, so "single audit path"
//!    confers no safety here that the local path lacks.
//!
//! # The features: frozen, deterministic, model-free
//!
//! [`embed`] is a lexical featureizer: hashed word tokens and character
//! trigrams into a fixed bucket vector, L2-normalized at the seam. It has no
//! model, no weights, no training and no network — it is *frozen* in exactly
//! the sense S5 asks for ("frozen features in, decisions out"), and it is
//! deterministic because nothing in it draws from entropy.
//!
//! It is a **lexical** featureizer and is not claimed to be a semantic one.
//! No accuracy, latency or quality figure for this ranking is asserted
//! anywhere; what is asserted is the *shape* of the result — a permutation of
//! the input — and that is what the tests check.
//!
//! # The config toggle
//!
//! [`ranking_enabled`] reads `MATHED_RANK` and is **off unless opted in**
//! (`unset`, ``, `0`, `off`, `false`, `no` all mean off). Off means the
//! caller keeps its original order byte-for-byte.

use decide_core::backbone::{Batch, EmbedSpec, feed, version_string};
use decide_core::consumers::ranked;
use decide_core::{DecideConfig, DecideCore, Mode};
use nalgebra::DVector;

/// Environment variable that turns editor ranking on. Off by default.
pub const RANK_ENV: &str = "MATHED_RANK";

/// The identity of the local featureizer as the engine sees it.
///
/// Named through the ordinary C7 `embed:` vocabulary so that this ranking
/// engine's statistics can never be confused with a real embedding model's —
/// they are two input spaces, and D7/A4 say that must be visible in the
/// version string rather than inferred.
pub const LOCAL_MODEL: &str = "mathed-rank/lexical-v1";

/// Feature width of [`embed`].
pub const DIM: usize = 128;

/// Projection width of the local engine (the papers' `D`, sized down to this
/// project's scale). Also the ceiling on how many classes can be fitted: the
/// ETF construction needs `features >= classes`, and anything beyond that
/// degrades to the caller's original order instead of erroring in a UI.
const PROJECTED: usize = 64;

/// Whether editor ranking is on, from the `MATHED_RANK` config variable.
///
/// Off unless opted in, so a deployment that says nothing gets exactly the
/// ordering it had before D10 existed.
pub fn ranking_enabled() -> bool {
    match std::env::var(RANK_ENV) {
        Ok(v) => enabled_from(&v),
        Err(_) => false,
    }
}

/// The toggle's value grammar, kept separate from the environment read so the
/// rule can be tested without mutating process-global state.
fn enabled_from(v: &str) -> bool {
    !matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "off" | "false" | "no"
    )
}

/// The embed spec for [`LOCAL_MODEL`], in the C7 vocabulary.
///
/// Built directly rather than through [`parse_embed_spec`] so this module
/// cannot panic on a constant — the two are tied together by
/// `local_spec_matches_the_parser`, which fails if they ever drift.
fn local_spec() -> EmbedSpec {
    EmbedSpec {
        backend: "local".to_string(),
        model: LOCAL_MODEL.to_string(),
        base_url: None,
        truncate_dim: None,
        instruction: None,
    }
}

/// FNV-1a: deterministic, seedless, identical on every host. A ranking that
/// changed order between two runs of the same document would not be
/// reproducible, and a golden test could not pin it.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A signed bucket: the sign keeps two colliding tokens from *always*
/// inflating the same coordinate, which would bias every dot product one way.
fn bucket(h: u64) -> (usize, f64) {
    let sign = if h & 1 == 0 { 1.0 } else { -1.0 };
    ((h >> 1) as usize % DIM, sign)
}

/// A frozen, deterministic, model-free text featureizer.
///
/// Word tokens and character trigrams are hashed into a fixed-width signed
/// bucket vector. Deliberately *not* normalized here: normalization belongs
/// to the seam ([`decide_core::backbone::feed`]), which is where a corpus and
/// a query are made to agree on what a vector means.
///
/// Never returns a zero vector — the seam refuses those — so a row whose text
/// carries no tokens still has a defined (unit) feature instead of failing the
/// whole batch.
pub fn embed(text: &str) -> DVector<f64> {
    let mut v = DVector::zeros(DIM);
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        let word = raw.to_lowercase();
        let (i, s) = bucket(fnv1a(word.as_bytes()));
        v[i] += s;
        let chars: Vec<char> = word.chars().collect();
        for w in chars.windows(3) {
            let gram: String = w.iter().collect();
            let (j, t) = bucket(fnv1a(gram.as_bytes()));
            v[j] += 0.5 * t;
        }
    }
    if v.norm() == 0.0 {
        v[0] = 1.0;
    }
    v
}

fn class_of(i: usize) -> String {
    format!("item-{i}")
}

fn identity(n: usize) -> Vec<usize> {
    (0..n).collect()
}

/// Order `labels`/`details` by similarity to the row at `focus`.
///
/// Returns a **permutation of `0..n`**: every index exactly once, so a caller
/// renders the same set in a different order. Whenever ranking cannot be done
/// honestly the original order is returned unchanged rather than a guess:
///
/// * fewer than two rows (a one-row list is already ordered);
/// * `focus` out of range, or the two slices disagree on length;
/// * the engine refuses to build, feed or fit — including more rows than the
///   ETF construction can span.
///
/// Each row is its own class, observed from its label and its detail, so the
/// focus row's prototype *is* the focus row and the others order by how close
/// they come to it: "related first". That is the whole claim — an order, with
/// no statement about correctness, offered as advice.
pub fn rank_rows(labels: &[&str], details: &[&str], focus: usize) -> Vec<usize> {
    let n = labels.len();
    if n < 2 || details.len() != n || focus >= n {
        return identity(n);
    }

    let spec = local_spec();
    let config = DecideConfig {
        embedding_version: version_string(&spec),
        embedding_dim: DIM,
        projected_dim: PROJECTED,
        heads: 1,
        mode: Mode::Ananc,
        lambda: 1.0,
        alpha: 1.0,
    };
    let Ok(mut core) = DecideCore::new(config) else {
        return identity(n);
    };
    for i in 0..n {
        if core.register(&class_of(i)).is_err() {
            return identity(n);
        }
    }

    // Two observations per class when the row has a detail: one sample is a
    // degenerate class mean, and the engine's own tests say so.
    let mut batch_labels = Vec::with_capacity(n * 2);
    let mut vectors = Vec::with_capacity(n * 2);
    for i in 0..n {
        batch_labels.push(class_of(i));
        vectors.push(embed(labels[i]));
        if !details[i].is_empty() {
            batch_labels.push(class_of(i));
            vectors.push(embed(details[i]));
        }
    }
    if feed(
        &mut core,
        &Batch {
            spec,
            labels: batch_labels,
            vectors,
        },
    )
    .is_err()
    {
        return identity(n);
    }
    if core.fit().is_err() {
        return identity(n);
    }

    let embeddings: Vec<DVector<f64>> = (0..n)
        .map(|i| embed(&format!("{} {}", labels[i], details[i])))
        .collect();
    let indices: Vec<usize> = (0..n).collect();
    let order: Vec<usize> = ranked(&core, &indices, &embeddings, &class_of(focus))
        .into_iter()
        .copied()
        .collect();

    // `ranked` is a permutation by construction; check it anyway, because a
    // UI that dropped or duplicated a row would be a different bug than the
    // one this function exists to avoid.
    if order.len() != n {
        return identity(n);
    }
    let mut seen = vec![false; n];
    for &i in &order {
        if i >= n || seen[i] {
            return identity(n);
        }
        seen[i] = true;
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two rows about one thing and one about another, so a focus on the
    /// shared subject has something to pull toward.
    fn fixture() -> (Vec<&'static str>, Vec<&'static str>) {
        (
            vec!["Gauge sector", "Renormalization", "Lattice spacing"],
            vec![
                "the field strength on the link variables",
                "the running of the coupling constant",
                "the discretization of the lattice",
            ],
        )
    }

    fn is_permutation(order: &[usize], n: usize) -> bool {
        order.len() == n && {
            let mut seen = vec![false; n];
            for &i in order {
                if i >= n || seen[i] {
                    return false;
                }
                seen[i] = true;
            }
            true
        }
    }

    #[test]
    fn the_local_spec_matches_what_the_parser_would_make() {
        let parsed = decide_core::backbone::parse_embed_spec(&format!("embed:{LOCAL_MODEL}"))
            .expect("LOCAL_MODEL is a valid embed spec");
        assert_eq!(local_spec(), parsed);
        assert_eq!(
            version_string(&local_spec()),
            format!("local:{LOCAL_MODEL}")
        );
    }

    #[test]
    fn embed_is_deterministic_and_never_zero() {
        let a = embed("the running of the coupling constant");
        let b = embed("the running of the coupling constant");
        assert_eq!(a, b, "same text must give the same vector, always");
        assert_eq!(a.len(), DIM);
        assert!(a.norm() > 0.0, "the seam refuses zero vectors");
        // Different text, different vector — otherwise every order is a tie
        // and the ranking would be a no-op dressed as a feature.
        assert_ne!(a, embed("lattice spacing discretization"));
    }

    #[test]
    fn embed_has_no_entropy_in_its_construction() {
        // The point of the FNV/signed-hash design: two calls in the same
        // process, two different strings, must differ *because of the text*
        // and not because of any state carried between calls.
        let x = embed("alpha");
        let y = embed("beta");
        let z = embed("alpha");
        assert_eq!(x, z);
        assert_ne!(x, y);
    }

    /// D10's acceptance criterion, stated on the return value: the SET is
    /// unchanged and only the order differs.
    #[test]
    fn ranking_returns_the_same_set_in_some_order() {
        let (labels, details) = fixture();
        for focus in 0..labels.len() {
            let order = rank_rows(&labels, &details, focus);
            assert!(
                is_permutation(&order, labels.len()),
                "focus {focus}: {order:?} is not a permutation of 0..{}",
                labels.len()
            );
        }
    }

    /// A reordered result is the feature; a list that never moves would make
    /// the whole exercise theatre.
    #[test]
    fn ranking_actually_reorders_something() {
        let (labels, details) = fixture();
        let order = rank_rows(&labels, &details, 2);
        assert!(
            order != (0..labels.len()).collect::<Vec<usize>>(),
            "expected a reorder, got the identity: {order:?}"
        );
        // Whatever the order, the focus row is the one it is ordered around,
        // so it must lead.
        assert_eq!(order[0], 2, "the focus row must come first: {order:?}");
    }

    #[test]
    fn too_few_rows_cannot_be_ordered_and_are_left_alone() {
        assert_eq!(rank_rows(&["only"], &["detail"], 0), vec![0]);
        assert_eq!(rank_rows(&[], &[], 0), Vec::<usize>::new());
    }

    #[test]
    fn a_focus_out_of_range_or_a_length_mismatch_is_left_alone() {
        let (labels, details) = fixture();
        assert_eq!(rank_rows(&labels, &details, 99), (0..3).collect::<Vec<_>>());
        assert_eq!(
            rank_rows(&labels, &["short"], 0),
            (0..3).collect::<Vec<_>>(),
            "a mismatched detail slice must not be silently zipped"
        );
    }

    /// The config toggle: off by default, on only when asked.
    #[test]
    fn ranking_is_off_unless_the_config_opts_in() {
        // No test sets RANK_ENV, so this asserts the default. Tests run in
        // one process, so mutating an env var here would race with every
        // other test rather than test anything.
        assert_eq!(RANK_ENV, "MATHED_RANK");
        // The parsing rule itself, exercised without touching the environment.
        for off in ["", "0", "off", "false", "no", "OFF", " 0 "] {
            assert!(
                !enabled_from(off),
                "{off:?} must mean off, so a default deployment keeps its order"
            );
        }
        for on in ["1", "on", "true", "yes", "anything-else"] {
            assert!(enabled_from(on), "{on:?} must mean on");
        }
    }
}
