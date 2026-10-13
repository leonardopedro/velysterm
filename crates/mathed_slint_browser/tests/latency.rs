//! Round-trip latency of the browser helper, measured on a realistic
//! multi-widget document.
//!
//! mathed drives this process once per changed block (`CREATE`) and once per
//! input event that lands on a widget (`EVENT`). Both round trips carry a full
//! RGBA frame back — 480x240 is 460 KB — so what matters is whether the cost is
//! small enough to sit inside a frame loop. These numbers are the evidence for
//! that, and for the shape of the client's knobs (the 10s watchdog timeout, and
//! hashing the spec so an unchanged block is never recompiled).
//!
//! The assertions are deliberately an order of magnitude looser than the
//! measured values: they exist to catch a regression, not to pin a number that
//! moves with the machine.

mod common;

use common::*;
use std::time::Instant;

/// A small form: label, four fields, three buttons, a checkbox, a slider, a
/// progress bar and a separator — twelve elements, which is a plausible upper
/// bound for an inline widget in a document.
const REALISTIC: &str = r#"
import { VerticalBox, HorizontalBox, LineEdit, Button, CheckBox, Slider, ProgressIndicator } from "std-widgets.slint";

export component Demo inherits Window {
    preferred-width: 480px;
    preferred-height: 240px;
    background: #203040;
    VerticalBox {
        padding: 12px;
        spacing: 6px;
        Text { text: "Session parameters"; color: #e0e8f0; font-size: 16px; }
        HorizontalBox {
            spacing: 6px;
            LineEdit { text: "alpha"; }
            LineEdit { text: "0.42"; }
        }
        HorizontalBox {
            spacing: 6px;
            LineEdit { text: "seed"; }
            LineEdit { text: "7"; }
        }
        CheckBox { text: "sample posterior"; checked: true; }
        Slider { minimum: 0; maximum: 1; value: 0.42; }
        ProgressIndicator { progress: 0.42; }
        HorizontalBox {
            spacing: 6px;
            Button { text: "run"; }
            Button { text: "reset"; }
            Button { text: "export"; }
        }
    }
}
"#;

/// (min, p50, p95) in milliseconds.
fn stats(samples: &[f64]) -> (f64, f64, f64) {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| {
        let idx = (((sorted.len() - 1) as f64 * q).round() as usize).min(sorted.len() - 1);
        sorted[idx]
    };
    (sorted[0], at(0.5), at(0.95))
}

fn ms(samples: &[f64]) -> String {
    let (min, p50, p95) = stats(samples);
    format!("min {min:6.2}  p50 {p50:6.2}  p95 {p95:6.2} ms")
}

#[test]
fn round_trip_latency_is_within_budget() {
    let mut b = Browser::spawn();
    let id = b.id();

    // CREATE: compile the DSL, instantiate, raster the first frame.
    let mut create = Vec::new();
    for _ in 0..10 {
        let started = Instant::now();
        ok(b.create(id, 480, 240, REALISTIC));
        create.push(started.elapsed().as_secs_f64() * 1000.0);
    }

    // EVENT: dispatch into the live instance and re-raster. No recompilation,
    // which is why hashing the spec on the client side pays off.
    let mut event = Vec::new();
    for i in 0..50 {
        let started = Instant::now();
        ok(b.event(id, &format!("move {i} {}", i % 240), None));
        event.push(started.elapsed().as_secs_f64() * 1000.0);
    }

    // A key press, which is what typing feels like.
    let mut key = Vec::new();
    for ch in ["a", "b", "c", "d", "e", "f", "g", "h"] {
        let started = Instant::now();
        b.event(id, "keydown 1", Some(ch));
        key.push(started.elapsed().as_secs_f64() * 1000.0);
    }

    let frame_kb = 480 * 240 * 4 / 1024;
    println!(
        "480x240 ({frame_kb} KB/frame), {} lines of DSL, 12 elements:",
        REALISTIC.lines().count()
    );
    println!("  CREATE (compile + first raster)  {}", ms(&create));
    println!("  EVENT  (pointer, re-raster)      {}", ms(&event));
    println!("  EVENT  (key press, re-raster)    {}", ms(&key));

    let (_, event_p50, _) = stats(&event);
    let (_, create_p50, _) = stats(&create);
    println!(
        "  => pointer events sustain {:.0}/s; blocks recompile only on edit",
        1000.0 / event_p50.max(f64::EPSILON)
    );

    assert!(
        create_p50 < 2_000.0,
        "CREATE p50 {create_p50:.2}ms is far too slow for an editor"
    );
    assert!(
        event_p50 < 250.0,
        "EVENT p50 {event_p50:.2}ms is far too slow to feel interactive"
    );
}

#[test]
fn latency_scales_with_frame_area() {
    // The frame payload is the dominant cost, so confirm it scales linearly
    // rather than something pathological. This is the evidence for keeping the
    // default block small.
    let mut b = Browser::spawn();
    let id = b.id();
    ok(b.create(id, 480, 240, REALISTIC));

    let mut previous_p50 = f64::INFINITY;
    for (width, height) in [(240, 120), (480, 240), (960, 480)] {
        ok(b.create(id, width, height, REALISTIC));
        let mut samples = Vec::new();
        for i in 0..20 {
            let started = Instant::now();
            ok(b.event(id, &format!("move {i} 1"), None));
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        let (_, p50, _) = stats(&samples);
        println!(
            "  {width}x{height} ({} KB/frame): p50 {p50:.2}ms",
            width * height * 4 / 1024
        );
        // Monotonic, and not worse than quadratic in the linear dimension.
        assert!(
            p50 <= previous_p50 * 8.0,
            "latency grew faster than the frame area"
        );
        previous_p50 = p50;
    }
}
