//! hl-agent binary: stdio MCP server. See README.md for wiring an agent host to it.
//! Flags:
//!   --config <path>   guardrail TOML (enables the simulate_order tool; required for it)
//!   --base <url>      Hyperliquid API base (default: mainnet public API)
//!   --testnet         use https://api.hyperliquid-testnet.xyz
//!   --check-config    validate the guardrail TOML and exit 0/2 (no server)
//!   --live            ENABLE the live-order tool (place_order_live). OFF by default: without
//!                     this flag the process holds no key and no signing code runs at all.
//!   --user <0x...>    the account the orders trade for (your MAIN wallet address); required by --live
//!   --builder <0x...> builder address to attach after YOUR approval (default: the kit maintainer's)
//!   --fee-tenths-bp N our builder fee, 1..=50 tenths of a bp (default 20 = 2 bp)
//! The agent key is NEVER a flag: --live reads it from the HL_AGENT_KEY environment variable only,
//! so it cannot land in argv, shell history or a process list.
use hl_agent::guardrail::Guardrail;
use hl_agent::mcp::Server;
use hl_agent::sim::DryRunSimulator;
use hl_agent::{HlClient, MAINNET, TESTNET};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut config: Option<String> = None;
    let mut base: Option<String> = None;
    let mut check_only = false;
    let mut live = false;
    let mut user: Option<String> = None;
    let mut builder: Option<String> = None;
    let mut fee_tenths_bp: u32 = 20;
    let mut approve_only = false;
    let mut print_example_config = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                i += 1;
                config = Some(args.get(i).cloned().unwrap_or_else(|| die("--config needs a path")));
            }
            "--base" => {
                i += 1;
                base = Some(args.get(i).cloned().unwrap_or_else(|| die("--base needs a url")));
            }
            "--testnet" => base = Some(TESTNET.into()),
            "--check-config" => check_only = true,
            "--live" => live = true,
            "--user" => {
                i += 1;
                user = Some(args.get(i).cloned().unwrap_or_else(|| die("--user needs a 0x address")));
            }
            "--builder" => {
                i += 1;
                builder = Some(args.get(i).cloned().unwrap_or_else(|| die("--builder needs a 0x address")));
            }
            "--approve-builder-fee" => approve_only = true,
            "--print-example-config" => print_example_config = true,
            "--fee-tenths-bp" => {
                i += 1;
                let raw = args.get(i).cloned().unwrap_or_else(|| die("--fee-tenths-bp needs a number"));
                fee_tenths_bp = raw
                    .parse()
                    .unwrap_or_else(|_| die("--fee-tenths-bp must be an integer 1..=50 (tenths of a basis point, 0.1 to 5 bp)"));
            }
            other => die(&format!("unknown flag: {other} (see --help in README.md)")),
        }
        i += 1;
    }

    // A prebuilt/`cargo install` user has no examples/ directory, but the README quickstart
    // hands them `examples/guardrail.example.toml`. Printing the same file from the binary
    // keeps that copy-paste from failing with "no such file".
    if print_example_config {
        print!("{}", include_str!("../examples/guardrail.example.toml"));
        return;
    }

    // The ONE signature that pays the maintainer. A user of the prebuilt archive has no
    // cargo and no examples/ directory, so this has to exist in the binary itself or the
    // documented no-toolchain install can never attach the builder code.
    if approve_only {
        approve_builder_fee_cmd(&base, builder.as_deref(), fee_tenths_bp, user.as_deref());
        return;
    }

    let cfg = match &config {
        Some(path) => match hl_agent::mcp::load_guardrail_config(path) {
            Ok(c) => c,
            Err(e) => die(&e),
        },
        None => {
            if live {
                // The live path signs real orders. Arming it on caps the user never wrote
                // down (built-in defaults) is not a default, it is a surprise: require the file.
                die("--live needs --config <guardrail.toml>: the caps that run before every signature must be the ones you wrote, not built-in defaults. examples/guardrail.example.toml is a starting point.");
            }
            eprintln!("hl-agent: no --config given: read-only market tools only (simulate_order disabled)");
            hl_agent::guardrail::GuardrailConfig::from_toml_str(defaults_toml()).expect("built-in defaults parse")
        }
    };

    if check_only {
        // Never let a stranger read built-in defaults as their own caps. The release notes
        // tell people to smoke-test with `--check-config`; with no file that command used to
        // print "guardrail config OK (caps: pos=1000 ...)" and nothing said the numbers came
        // from the binary, not from a guardrail.toml they wrote.
        if config.is_some() {
            println!("{}", check_config_report(Some(&cfg)));
        } else {
            println!("{}", check_config_report(None));
        }
        return;
    }

    let base_url = base.unwrap_or_else(|| MAINNET.into());
    let hl = HlClient::new(&base_url);
    let sim = config
        .as_ref()
        .map(|_| DryRunSimulator::new(Guardrail::new(cfg.clone())));
    let mut server = Server::new(hl, sim);
    if live {
        // Build the live path FIRST: a bad key or a missing --user must exit before
        // we print "serving", so a user never sees a server that cannot do the job.
        server = server.with_live(build_live(&base_url, cfg, user, builder, fee_tenths_bp));
    }
    eprintln!(
        "hl-agent: serving MCP over stdio; api={base_url}; simulate_order={}; place_order_live={}",
        if config.is_some() { "enabled" } else { "disabled (no --config)" },
        if live { "ENABLED" } else { "off (start with --live to sign)" }
    );

    if let Err(e) = server.serve() {
        eprintln!("hl-agent: stdio loop ended: {e}");
        std::process::exit(1);
    }
}

/// The kit maintainer's builder address. Fee income for the project; attached only
/// after the user's own main wallet approves it on the venue (revocable any time).
const DEFAULT_BUILDER: &str = "0x5a229a20D9636a07583D6868FABf4B2903271Ab7";

/// Build the live-order path. Every requirement is checked HERE, before the server
/// takes a single request: a misconfigured --live exits 2 with one sentence instead
/// of failing after a user's order was already in flight.
///
/// The agent key comes from HL_AGENT_KEY only (never argv, never a file this kit
/// writes). It is a Hyperliquid API wallet key: sign-only, cannot withdraw, cannot
/// approve anything. `--user` is the MAIN wallet whose account the orders trade for;
/// the venue compares the two, so a wrong pair fails on the first order with a clear
/// error instead of silently trading the wrong account.
fn build_live(
    base_url: &str,
    cfg: hl_agent::guardrail::GuardrailConfig,
    user: Option<String>,
    builder: Option<String>,
    fee_tenths_bp: u32,
) -> Box<dyn hl_agent::live::LiveTool> {
    let user = user.unwrap_or_else(|| {
        die("--live needs --user <your MAIN wallet 0x address>: the account your agent key trades for")
    });
    if !user.starts_with("0x") || user.len() != 42 {
        die(&format!("--user looks wrong ({user}): expected a 42-character 0x address"));
    }
    if !(1..=50).contains(&fee_tenths_bp) {
        die(&format!("--fee-tenths-bp {fee_tenths_bp} is outside 1..=50 (0.1 to 5 bp), the range this kit asks; the venue caps perps at 10 bp"));
    }
    let key = std::env::var("HL_AGENT_KEY").unwrap_or_else(|_| {
        die("--live needs HL_AGENT_KEY in the environment (your Hyperliquid API-wallet key). It is never read from a flag, a file or .env by this kit.")
    });
    let signer = hl_agent::AgentSigner::from_hex_key(&key)
        // Discard the library error: alloy quotes the offending character of the key.
        .unwrap_or_else(|_| die("HL_AGENT_KEY is not a usable 0x-prefixed 32-byte hex key. Nothing was signed."));
    // Default builder: the kit maintainer's address, so a user who follows the README
    // earns us the fee we ask for. It only ever RIDES on an order after the venue
    // reports the user approved us — no approval, no builder field, order unaffected.
    let builder = builder
        .or_else(|| std::env::var("HL_BUILDER_ADDRESS").ok())
        .or_else(|| Some(DEFAULT_BUILDER.to_string()));
    let is_mainnet = base_url == MAINNET;
    // Same allowlist as the approve command: "off" or "0" is not consent for real orders.
    if is_mainnet && !std::env::var("HL_ALLOW_MAINNET").ok().map(|v| mainnet_opt_in(&v)).unwrap_or(false) {
        die("mainnet orders need HL_ALLOW_MAINNET=1 in the environment (only 1, true, yes or on count). Start on --testnet with a throwaway key.");
    }
    // PROVE the account before arming: the signed action carries no user field, so the
    // venue cannot catch a wrong --user. A wrong one (a typo, the API wallet's own
    // address, a subaccount) returns a well-formed EMPTY account, which would leave the
    // position and daily-loss caps reading zero while orders still execute on the master.
    // So: the signer's own address OR an entry in the main wallet's extraAgents, or exit.
    let ex = hl_agent::ExchangeClient::new(base_url, signer);
    let signer_addr = ex.signer_address().to_ascii_lowercase();
    let user_lc = user.to_ascii_lowercase();
    if signer_addr != user_lc {
        let hl_check = HlClient::new(base_url);
        match hl_check.extra_agents(&user) {
            Ok(agents) if agents.iter().any(|a| a.eq_ignore_ascii_case(&signer_addr)) => {}
            Ok(_) => die(&format!(
                "--user {user} does not list this key's address {signer_addr} in its extraAgents, so the key may not trade for that account and the guardrail caps would read the wrong one. Approve the API wallet at app.hyperliquid.xyz/API for that main wallet, then retry."
            )),
            Err(e) => die(&format!(
                "could not read extraAgents for --user {user} ({e}), so the kit cannot prove this key may trade for it — refusing to arm rather than read caps from the wrong account"
            )),
        }
    }
    let venue = hl_agent::HlVenue { hl: HlClient::new(base_url), ex };
    let live_cfg = hl_agent::LiveConfig {
        user,
        builder: builder.clone(),
        fee_tenths_bp,
        is_mainnet,
    };
    let gate = Guardrail::new(cfg);
    eprintln!(
        "hl-agent: LIVE ORDER PATH ARMED on {}; builder fee {} tenths of a bp; guardrail caps run before every signature",
        if is_mainnet { "MAINNET" } else { "testnet" },
        fee_tenths_bp
    );
    match &builder {
        Some(b) => eprintln!("hl-agent: builder {b} — attaches only if your main wallet approved a fee >= {fee_tenths_bp} on the venue (examples/approve_builder_fee.rs)"),
        None => eprintln!("hl-agent: NO builder address configured — orders carry no builder fee"),
    }
    Box::new(hl_agent::LiveTrader::new(venue, live_cfg, gate))
}

fn die(msg: &str) -> ! {

    eprintln!("hl-agent error: {msg}");
    std::process::exit(2);
}

/// Conservative built-in defaults when no --config is passed (simulate_order still
/// disabled without --config; these only matter if a future flag enables it).
/// The `--check-config` line. Says whose caps are being reported, so the built-in
/// defaults can never be mistaken for a guardrail file the user wrote. PURE.
fn check_config_report(cfg: Option<&hl_agent::guardrail::GuardrailConfig>) -> String {
    let caps = |c: &hl_agent::guardrail::GuardrailConfig| format!(
        "pos={:.2}usd order={:.2}usd lev={}x loss={:.2}usd tripwire={}/{}s rate={}/h",
        c.max_position_notional_usd, c.max_order_notional_usd, c.max_leverage,
        c.daily_loss_cap_usd, c.max_similar_actions, c.loop_window_seconds, c.max_calls_per_hour);
    match cfg {
        Some(c) => format!("hl-agent: guardrail config OK (caps: {})", caps(c)),
        None => format!(
            "hl-agent: no --config given, so there is no guardrail file to check. These are the BUILT-IN defaults, not caps you wrote: {}. Pass --config <guardrail.toml> (examples/guardrail.example.toml is a starting point) and --live will refuse to run without one.",
            caps(&hl_agent::guardrail::GuardrailConfig::from_toml_str(defaults_toml()).expect("built-in defaults parse"))),
    }
}

fn defaults_toml() -> &'static str {
    "max_position_notional_usd = 1000.0\nmax_order_notional_usd = 100.0\nmax_leverage = 1.0\naccount_equity_usd = 500.0\ndaily_loss_cap_usd = 20.0\nmax_similar_actions = 3\nloop_window_seconds = 60\nmax_calls_per_hour = 10\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--check-config` with no `--config` must not print the built-in default caps as if
    /// they were the user's guardrail file. The release notes tell a stranger to smoke
    /// test with this command; before this, it printed "guardrail config OK (caps: ...)"
    /// over defaults from the binary and nothing said so.
    #[test]
    fn check_config_without_a_file_never_presents_defaults_as_the_users_caps() {
        let out = check_config_report(None);
        assert!(out.contains("BUILT-IN"), "must name them built-in defaults: {out}");
        assert!(out.contains("no --config given"), "must say no file was given: {out}");
        assert!(!out.contains("config OK"), "must not read as a passed check: {out}");

        let cfg = hl_agent::guardrail::GuardrailConfig::from_toml_str(
            "max_position_notional_usd = 42.0\nmax_order_notional_usd = 10.0\nmax_leverage = 1.0\naccount_equity_usd = 100.0\ndaily_loss_cap_usd = 5.0\nmax_similar_actions = 3\nloop_window_seconds = 60\nmax_calls_per_hour = 10\n",
        )
        .expect("valid toml");
        let with_file = check_config_report(Some(&cfg));
        assert!(with_file.contains("config OK"), "a real file still reports OK: {with_file}");
        assert!(!with_file.contains("BUILT-IN"), "a real file is not built-in: {with_file}");
        assert!(with_file.contains("pos=42.00usd"), "quotes the user's own number: {with_file}");
    }
}


/// Strict consent, as an ALLOWLIST: only "1", "true", "yes" or "on" unlock mainnet signing.
/// A denylist was wrong and a critic proved it: `HL_ALLOW_MAINNET=off` slipped through and
/// signed a real mainnet approval. Anything unrecognised (empty, "0", "off", a typo, an
/// exported-but-empty var) must refuse: the cost of the wrong answer is a MAIN wallet signature
/// over real funds.
fn mainnet_opt_in(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// Which network a approval targets. Testnet must be named exactly; anything else,
/// including a custom --base URL, is treated as MAINNET and so needs the opt-in (fail-closed).
fn is_testnet(base_url: &str) -> bool {
    base_url.trim_end_matches('/') == TESTNET.trim_end_matches('/')
}

/// `--approve-builder-fee`: the user's ONE signature, available in the shipped binary.
///
/// The prebuilt archive is a bare executable: no cargo, no `examples/` directory. Before
/// this command existed, the only way to run `approveBuilderFee` was `cargo run --example`,
/// so a user who took the "no Rust toolchain needed" install could never attach our builder
/// code — the documented install path could not pay the maintainer at all.
///
/// The user's MAIN wallet signs here. That key controls the account's funds, so it is read
/// from HL_MAIN_KEY only (never a flag: argv lands in shell history and `ps`), used in
/// memory, never printed, and the process exits as soon as the action is confirmed. The default
/// base URL is MAINNET; --testnet switches to the testnet API, and every mainnet signature
/// additionally needs HL_ALLOW_MAINNET=1. So --user is rejected here: the key signs for itself.
fn approve_builder_fee_cmd(base: &Option<String>, builder: Option<&str>, fee_tenths_bp: u32, user: Option<&str>) {
    // ApproveBuilderFee is signed BY the approving wallet, for itself. A --user here would be
    // silently ignored and invites the wrong-account mistake the live path guards against.
    if let Some(u) = user {
        die(&format!("--user {u} is not used by --approve-builder-fee: your own main wallet signs this, for that same address. Drop --user."));
    }
    let base_url = base.clone().unwrap_or_else(|| MAINNET.to_string());
    // `--testnet` sets the testnet base URL; anything else is mainnet and needs the opt-in.
    let on_testnet = is_testnet(&base_url);
    if !(1..=50).contains(&fee_tenths_bp) {
        die(&format!("--fee-tenths-bp {fee_tenths_bp} is outside 1..=50 (0.1 to 5 bp), the range this kit asks; the venue caps perps at 10 bp"));
    }
    let builder_addr = builder.unwrap_or(DEFAULT_BUILDER).to_string();
    if hl_agent::validate_builder_address(&builder_addr).is_err() {
        die(&format!("--builder {builder_addr} is not a usable 0x address"));
    }
    let key = std::env::var("HL_MAIN_KEY").unwrap_or_else(|_| {
        die("--approve-builder-fee needs HL_MAIN_KEY in the environment (your MAIN wallet key, the one that can withdraw). Read it without echoing: read -rs HL_MAIN_KEY && export HL_MAIN_KEY")
    });
    let key = key.trim().to_string();
    if key.is_empty() {
        die("HL_MAIN_KEY is empty. Read it without echoing: read -rs HL_MAIN_KEY && export HL_MAIN_KEY");
    }
    // Never echo the key or a library error that quoted it: keep the diagnosis, drop the value.
    let signer = hl_agent::AgentSigner::from_hex_key(&key).unwrap_or_else(|_| {
        die("HL_MAIN_KEY is not a usable 0x-prefixed 32-byte hex key. Nothing was signed.")
    });
    // Only now that a real key exists does the mainnet opt-in matter: asking for it first
    // hides the key error from a user who simply has not exported it yet.
    let mainnet_ok = std::env::var("HL_ALLOW_MAINNET").ok().map(|v| mainnet_opt_in(&v)).unwrap_or(false);
    if !on_testnet && !mainnet_ok {
        die("--approve-builder-fee on mainnet needs HL_ALLOW_MAINNET=1 in the environment. Rehearse on --testnet from the account that will trade; the venue refuses approvals from an address it has never seen (see the Multi-sig note in README.md).");
    }
    let chain = if on_testnet { "testnet" } else { "MAINNET" };
    let user = signer.address();

    // Read the venue BEFORE signing: never spend a signature on a network we cannot reach.
    if HlClient::new(&base_url).all_mids().is_err() {
        die(&format!("{chain} API did not answer before signing; nothing was signed or sent. Retry when the venue is reachable."));
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_else(|_| die("system clock is before 1970; refusing to sign a nonce"));
    let client = hl_agent::ExchangeClient::new(&base_url, signer);
    let action = hl_agent::ExchangeClient::approve_builder_fee_action(&builder_addr, fee_tenths_bp, nonce, !on_testnet)
        .unwrap_or_else(|e| die(&format!("could not build the approveBuilderFee action: {e}")));
    eprintln!("hl-agent: {chain}; main wallet {user} approves builder {builder_addr} up to {} bp per fill", f64::from(fee_tenths_bp) / 10.0);
    eprintln!("hl-agent: signed action (the exact bytes being sent): {action}");
    let resp = client
        .approve_builder_fee(&builder_addr, fee_tenths_bp, nonce, !on_testnet)
        .unwrap_or_else(|e| die(&format!("approveBuilderFee request failed: {e}")));
    if !resp.is_ok() {
        // The venue decides. New, empty accounts are refused with "Multi-sig required", which
        // is not about multi-sig: it is the venue declining an account it cannot act on yet.
        let reason = resp.error_text();
        let hint = if reason.to_ascii_lowercase().contains("multi-sig") {
            " (observed 2026-11-10 on a never-used throwaway wallet. If your account is funded and you still see this, the venue is not accepting a single-key approval for it, and this command cannot help: use a route the venue accepts)"
        } else {
            ""
        };
        die(&format!("the venue refused the approval: {reason}{hint}"));
    }
    // Prove it landed with a keyless read of the venue's own state, not with our response.
    match client.max_builder_fee(&user.to_string(), &builder_addr) {
        Ok(max) if max >= fee_tenths_bp => {
            println!(
                "PASS: maxBuilderFee({user}, {builder_addr}) = {max} tenths of bp — the builder may charge up to {} bp per fill, revocable any time from your account page.",
                f64::from(max) / 10.0
            );
        }
        Ok(max) => {
            die(&format!("the venue accepted the action but maxBuilderFee reads {max} < {fee_tenths_bp}; the approval is not active. Nothing else was sent."));
        }
        Err(e) => {
            die(&format!("the approval was accepted but could not be re-read from the venue ({e}); check maxBuilderFee in the UI before relying on it."));
        }
    }
}

#[cfg(test)]
mod approve_cmd_tests {
    use super::*;

    /// A custom `--base` must NOT be treated as a rehearsal: only the exact testnet URL is
    /// testnet, everything else (including a typo or a proxy) is mainnet and so needs the
    /// opt-in. Fail-closed matters because the wrong answer spends a MAIN wallet signature.
    #[test]
    fn only_the_exact_testnet_url_is_a_rehearsal() {
        assert!(is_testnet(TESTNET));
        assert!(is_testnet(&format!("{TESTNET}/")), "a trailing slash is the same venue");
        assert!(!is_testnet(MAINNET));
        assert!(!is_testnet("https://my-proxy.example/hl"), "a custom base is not a rehearsal");
        assert!(!is_testnet("https://api.hyperliquid-testnet.xyz.evil.example"), "a lookalike host is not testnet");
        assert!(!is_testnet(""));
    }

    /// `HL_ALLOW_MAINNET` is consent for a real, revocable, on-chain permission on real funds.
    /// Exported-but-empty, "0", "false" and "no" are NOT consent. This is the gate; get it
    /// wrong and a user signs on mainnet without ever saying yes.
    #[test]
    fn the_mainnet_opt_in_needs_a_real_yes() {
        for yes in ["1", "true", "YES", "yes", "on", " 1 "] {
            assert!(mainnet_opt_in(yes), "{yes:?} is a yes");
        }
        for no in ["", " ", "0", "false", "FALSE", "no", "No", "off", "OFF", "disabled", "2", "y"] {
            assert!(!mainnet_opt_in(no), "{no:?} is not consent");
        }
    }

}
