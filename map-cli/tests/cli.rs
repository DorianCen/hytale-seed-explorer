//! T11 CLI tests: argument handling only — no generator load here, so they
//! run everywhere, including machines without the worldgen checkout.
//!
//! `--help` proves the `render` subcommand is wired; the invalid-seed case
//! proves seed errors (T02 semantics) are reported as clean CLI failures
//! before the worker pool ever starts.

use std::process::Command;

/// The just-built `map-cli` binary cargo provides to integration tests.
fn map_cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_map-cli"))
}

#[test]
fn help_lists_the_render_command_and_its_flags() {
    let output = map_cli().arg("--help").output().expect("map-cli runs");
    assert!(output.status.success(), "--help must exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("render"), "the render subcommand is listed");

    // The render-specific flags are documented on the subcommand's help.
    let output = map_cli()
        .args(["render", "--help"])
        .output()
        .expect("map-cli runs");
    assert!(output.status.success(), "render --help must exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for flag in ["--seed", "--center", "--size", "--step", "--layer", "--out"] {
        assert!(stdout.contains(flag), "render --help documents {flag}");
    }
}

#[test]
fn invalid_seed_fails_cleanly_without_loading_a_world() {
    let output = map_cli()
        .args([
            "render",
            "--seed",
            "not-a-number",
            "--center",
            "0,0",
            "--size",
            "8",
            "--step",
            "1",
            "--layer",
            "biome",
            "--out",
            "/tmp/opencode/t11-invalid-seed.png",
        ])
        .output()
        .expect("map-cli runs");
    assert!(
        !output.status.success(),
        "a non-numeric seed must be a CLI error, not a render"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("seed"),
        "the error must name the seed: {stderr}"
    );
    // The seed is validated before the pool starts, so no world load line.
    assert!(
        !stderr.contains("loading world"),
        "seed errors must not pay the generator load: {stderr}"
    );
    // T02: seeds are numeric only, never hashed as a fallback.
    assert!(
        !stderr.contains("loading world ../"),
        "no partial world resolution before the seed check: {stderr}"
    );
}

#[test]
fn unknown_layer_is_rejected() {
    let output = map_cli()
        .args([
            "render",
            "--seed",
            "0",
            "--layer",
            "no-such-layer",
            "--out",
            "/tmp/opencode/t11-bad-layer.png",
        ])
        .output()
        .expect("map-cli runs");
    assert!(
        !output.status.success(),
        "an unknown layer must be a CLI error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("biome"),
        "the error should mention the only valid layer: {stderr}"
    );
}

#[test]
fn malformed_center_is_rejected() {
    let output = map_cli()
        .args([
            "render",
            "--seed",
            "0",
            "--center",
            "12",
            "--out",
            "/tmp/opencode/t11-bad-center.png",
        ])
        .output()
        .expect("map-cli runs");
    assert!(
        !output.status.success(),
        "a center without both coordinates must be a CLI error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--center"),
        "the error must name the flag: {stderr}"
    );
}
