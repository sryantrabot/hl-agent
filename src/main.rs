//! hl-agent binary: stdio MCP server. See README.md for wiring an agent host to it.
//! Flags:
//!   --config <path>   guardrail TOML (enables the simulate_order tool; required for it)
//!   --base <url>      Hyperliquid API base (default: mainnet public API)
//!   --testnet         use https://api.hyperliquid-testnet.xyz
//!   --check-config    validate the guardrail TOML and exit 0/2 (no server)
//! NO key flag exists: week 1 never signs anything.
use hl_agent::guardrail::Guardrail;
use hl_agent::mcp::Server;
use hl_agent::sim::DryRunSimulator;
use hl_agent::{HlClient, MAINNET, TESTNET};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut config: Option<String> = None;
    let mut base: Option<String> = None;
    let mut check_only = false;
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
            eprintln!("hl-agent: no --config given: read-only market tools only (simulate_order disabled)");
            hl_agent::guardrail::GuardrailConfig::from_toml_str(defaults_toml()).expect("built-in defaults parse")
        }
    };

    if check_only {
        println!("hl-agent: guardrail config OK (caps: pos={:.2}usd order={:.2}usd lev={}x loss={:.2}usd tripwire={}/{}s rate={}/h)",
            cfg.max_position_notional_usd, cfg.max_order_notional_usd, cfg.max_leverage,
            cfg.daily_loss_cap_usd, cfg.max_similar_actions, cfg.loop_window_seconds, cfg.max_calls_per_hour);
        return;
    }

    let base_url = base.unwrap_or_else(|| MAINNET.into());
    eprintln!("hl-agent: serving MCP over stdio; api={base_url}; simulate_order={}", if config.is_some() { "enabled" } else { "disabled (no --config)" });
    let hl = HlClient::new(&base_url);
    let sim = config.map(|_| DryRunSimulator::new(Guardrail::new(cfg)));
    let mut server = Server::new(hl, sim);
    if let Err(e) = server.serve() {
        eprintln!("hl-agent: stdio loop ended: {e}");
        std::process::exit(1);
    }
}

fn die(msg: &str) -> ! {
    eprintln!("hl-agent error: {msg}");
    std::process::exit(2);
}

/// Conservative built-in defaults when no --config is passed (simulate_order still
/// disabled without --config; these only matter if a future flag enables it).
fn defaults_toml() -> &'static str {
    "max_position_notional_usd = 1000.0\nmax_order_notional_usd = 100.0\nmax_leverage = 1.0\naccount_equity_usd = 500.0\ndaily_loss_cap_usd = 20.0\nmax_similar_actions = 3\nloop_window_seconds = 60\nmax_calls_per_hour = 10\n"
}
