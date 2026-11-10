//! Behavioural test of the shipped binary itself, not a grep of its source.
//!
//! The v0.2.0 release archive is a BARE EXECUTABLE: no cargo, no `examples/` directory. The
//! only step that attaches the builder code — the one `approveBuilderFee` signature — used to
//! require `cargo run --example`, so a stranger who took the advertised "no Rust toolchain
//! needed" install could never pay the maintainer. The command now lives in the binary, and
//! this test proves the dispatch actually happens by RUNNING the binary.

use std::process::Command;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_hl-agent")
}

/// `--approve-builder-fee` must reach the approval command, not fall through to the MCP
/// server. With no key exported it exits non-zero and names HL_MAIN_KEY; if the flag stopped
/// being dispatched the process would instead start serving MCP and print "serving MCP".
#[test]
fn approve_flag_reaches_the_command_instead_of_starting_the_server() {
    let out = Command::new(binary())
        .args(["--testnet", "--approve-builder-fee"])
        .env_remove("HL_MAIN_KEY")
        .env_remove("HL_ALLOW_MAINNET")
        .output()
        .expect("the binary must run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "it must not report success with no key: {stderr}");
    assert!(stderr.contains("HL_MAIN_KEY"), "it must tell the user which variable is missing: {stderr}");
    assert!(
        !stderr.contains("serving MCP"),
        "the flag must not fall through to the MCP server: {stderr}"
    );
    assert!(out.stdout.is_empty(), "nothing should be printed to stdout on this path: {:?}", out.stdout);
}

/// `--print-example-config` has to work with no checkout: that is the whole point. Its output
/// must be a guardrail file the same binary accepts back through `--check-config`.
#[test]
fn the_binary_can_emit_a_guardrail_file_it_accepts() {
    let printed = Command::new(binary())
        .arg("--print-example-config")
        .output()
        .expect("the binary must run");
    assert!(printed.status.success(), "printing the config must succeed");
    let toml = String::from_utf8_lossy(&printed.stdout);
    for key in [
        "max_position_notional_usd",
        "max_order_notional_usd",
        "max_leverage",
        "daily_loss_cap_usd",
    ] {
        assert!(toml.contains(key), "the emitted file must contain {key}: {toml}");
    }
    let dir = std::env::temp_dir().join(format!("hl-agent-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("guardrail.toml");
    std::fs::write(&path, toml.as_bytes()).expect("write config");
    let checked = Command::new(binary())
        .arg("--check-config")
        .arg("--config")
        .arg(&path)
        .output()
        .expect("the binary must run");
    let out = String::from_utf8_lossy(&checked.stdout);
    assert!(checked.status.success(), "the emitted config must validate: {out}");
    assert!(out.contains("config OK"), "a real file must report OK: {out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A mainnet approval is a real, revocable, on-chain permission over real funds. Without the
/// opt-in the binary must refuse BEFORE signing, and an exported-but-empty variable is not a
/// yes. The key is a throwaway: this test never has funds behind it and must not sign.
#[test]
fn mainnet_refuses_without_a_real_opt_in() {
    let throwaway = format!("0x{}", "22".repeat(32));
    for opt_in in [None, Some(""), Some("0"), Some("false"), Some("off"), Some("disabled")] {
        let mut cmd = Command::new(binary());
        cmd.args(["--approve-builder-fee", "--fee-tenths-bp", "20"])
            .env("HL_MAIN_KEY", &throwaway)
            .env_remove("HL_ALLOW_MAINNET");
        if let Some(v) = opt_in {
            cmd.env("HL_ALLOW_MAINNET", v);
        }
        let out = cmd.output().expect("the binary must run");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "opt-in {opt_in:?} must not be accepted");
        assert!(
            stderr.contains("HL_ALLOW_MAINNET"),
            "it must say the opt-in is what is missing for {opt_in:?}: {stderr}"
        );
        assert!(!stderr.contains(&throwaway), "the key must never be echoed: {stderr}");
    }
}
