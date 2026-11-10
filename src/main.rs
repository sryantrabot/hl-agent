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
//!   --fee-tenths-bp N our builder fee, 1..=100 tenths of a bp (default 20 = 2 bp)
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
            "--fee-tenths-bp" => {
                i += 1;
                let raw = args.get(i).cloned().unwrap_or_else(|| die("--fee-tenths-bp needs a number"));
                fee_tenths_bp = raw
                    .parse()
                    .unwrap_or_else(|_| die("--fee-tenths-bp must be an integer 1..=100 (tenths of a basis point)"));
            }
            other => die(&format!("unknown flag: {other} (see --help in README.md)")),
        }
        i += 1;
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
    if !(1..=100).contains(&fee_tenths_bp) {
        die(&format!("--fee-tenths-bp {fee_tenths_bp} is outside 1..=100 (0.1 to 10 bp); the venue caps perps at 10 bp"));
    }
    let key = std::env::var("HL_AGENT_KEY").unwrap_or_else(|_| {
        die("--live needs HL_AGENT_KEY in the environment (your Hyperliquid API-wallet key). It is never read from a flag, a file or .env by this kit.")
    });
    let signer = hl_agent::AgentSigner::from_hex_key(&key)
        .unwrap_or_else(|e| die(&format!("HL_AGENT_KEY is not a usable key: {e}")));
    // Default builder: the kit maintainer's address, so a user who follows the README
    // earns us the fee we ask for. It only ever RIDES on an order after the venue
    // reports the user approved us — no approval, no builder field, order unaffected.
    let builder = builder
        .or_else(|| std::env::var("HL_BUILDER_ADDRESS").ok())
        .or_else(|| Some(DEFAULT_BUILDER.to_string()));
    let is_mainnet = base_url == MAINNET;
    if is_mainnet && std::env::var("HL_ALLOW_MAINNET").is_err() {
        die("mainnet orders need HL_ALLOW_MAINNET set in the environment. Start on --testnet with a throwaway key.");
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
