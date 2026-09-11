use chrono::{DateTime, Duration, Utc};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

// ============================================================================
// CLI
// ============================================================================

#[derive(Parser, Debug)]
#[command(
    name = "nightscout-waybar",
    about = "Waybar module for NightScout CGM data"
)]
struct Cli {
    /// NightScout base URL (e.g. http://localhost:1337)
    url: String,

    /// Path to config file (TOML)
    #[arg(short = 'c', long = "config")]
    config: Option<PathBuf>,
}

// ============================================================================
// Configuration
// ============================================================================

#[derive(Deserialize, Debug, Clone)]
#[serde(default)]
struct Config {
    cache_file_path: String,
    bg: BgConfig,
    pump: PumpConfig,
    transmitter: TransmitterConfig,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(default)]
struct BgConfig {
    low_warn: f64,
    high_warn: f64,
    low_crit: f64,
    high_crit: f64,
    stale_mins: i64,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(default)]
struct PumpConfig {
    res_show: f64,
    res_warn: f64,
    res_crit: f64,
    expiry_warn_hours: i64,
    expiry_crit_hours: i64,
    lifetime_hours: i64,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(default)]
struct TransmitterConfig {
    expiry_warn_hours: i64,
    expiry_crit_hours: i64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cache_file_path: "/tmp/waybar_nightscout_state.json".to_string(),
            bg: BgConfig::default(),
            pump: PumpConfig::default(),
            transmitter: TransmitterConfig::default(),
        }
    }
}

impl Default for BgConfig {
    fn default() -> Self {
        Self {
            low_warn: 4.0,
            high_warn: 10.0,
            low_crit: 3.3,
            high_crit: 14.0,
            stale_mins: 15,
        }
    }
}

impl Default for PumpConfig {
    fn default() -> Self {
        Self {
            res_show: 50.0,
            res_warn: 50.0,
            res_crit: 30.0,
            expiry_warn_hours: 24,
            expiry_crit_hours: 2,
            lifetime_hours: 72,
        }
    }
}

impl Default for TransmitterConfig {
    fn default() -> Self {
        Self {
            expiry_warn_hours: 24,
            expiry_crit_hours: 3,
        }
    }
}

impl Config {
    fn load(path: Option<&PathBuf>) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        let Ok(contents) = fs::read_to_string(path) else {
            eprintln!(
                "Warning: could not read config file {:?}, using defaults",
                path
            );
            return Self::default();
        };
        match toml::from_str(&contents) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("Warning: failed to parse config: {e}, using defaults");
                Self::default()
            }
        }
    }
}

// ============================================================================
// Shared types
// ============================================================================

/// Output contributed by a single module.
struct ModuleOutput {
    status_line: String,
    tooltip_lines: Vec<String>,
    css_class: String,
    notifications: Vec<Notification>,
    cache_state: serde_json::Value,
}

impl ModuleOutput {
    fn empty() -> Self {
        Self {
            status_line: String::new(),
            tooltip_lines: Vec::new(),
            css_class: "normal".to_string(),
            notifications: Vec::new(),
            cache_state: serde_json::json!({}),
        }
    }
}

struct Notification {
    message: String,
    critical: bool,
}

#[derive(Serialize)]
struct WaybarOutput {
    text: String,
    tooltip: String,
    class: String,
}

const NORMAL: &str = "normal";
const WARNING: &str = "warning";
const CRITICAL: &str = "critical";
const ERROR: &str = "error";

fn state_rank(state: &str) -> i32 {
    match state {
        CRITICAL => 2,
        WARNING => 1,
        _ => 0,
    }
}

/// True if `current` is a degradation compared to `previous`.
fn is_degradation(previous: &str, current: &str) -> bool {
    state_rank(current) > state_rank(previous)
}

// ============================================================================
// HTTP helpers
// ============================================================================

fn http_get_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    agent
        .get(url)
        .call()
        .map_err(|e| e.to_string())?
        .into_json::<T>()
        .map_err(|e| e.to_string())
}

// ============================================================================
// Time helpers
// ============================================================================

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Format a duration (in hours) as e.g. `1d5h` or `12h`.
fn format_hours(hours: f64) -> (String, String) {
    if hours <= 0.0 {
        return ("EXPIRED".to_string(), "Expired".to_string());
    }
    let total_hours = hours.floor() as i64;
    let days = total_hours / 24;
    let rem = total_hours % 24;
    if days > 0 {
        (format!("{days}d{rem}h"), format!("{days}d{rem}h"))
    } else {
        (format!("{total_hours}h"), format!("{total_hours}h"))
    }
}

// ============================================================================
// BG module
// ============================================================================

#[derive(Deserialize)]
struct Entry {
    sgv: f64,
    date: i64,
    #[serde(default)]
    direction: Option<String>,
}

fn arrow_for(direction: &str) -> &'static str {
    match direction {
        "DoubleUp" => "↑↑",
        "SingleUp" => "↑",
        "FortyFiveUp" => "↗",
        "Flat" => "→",
        "FortyFiveDown" => "↘",
        "SingleDown" => "↓",
        "DoubleDown" => "↓↓",
        "NOT COMPUTABLE" => "?",
        "RATE OUT OF RANGE" => "‼",
        _ => "↺",
    }
}

fn run_bg_module(url: &str, cfg: &BgConfig, prev_state: &str) -> ModuleOutput {
    let mut out = ModuleOutput::empty();
    let endpoint = format!("{url}/api/v1/entries/current.json");

    let entries: Vec<Entry> = match http_get_json(&endpoint) {
        Ok(v) => v,
        Err(_) => {
            out.status_line = "NS: API ERR".to_string();
            out.tooltip_lines
                .push("Failed to fetch data from NightScout".to_string());
            out.css_class = ERROR.to_string();
            return out;
        }
    };

    let Some(entry) = entries.into_iter().next() else {
        out.status_line = "NS: NO DATA".to_string();
        out.tooltip_lines
            .push("No entries found in NightScout response".to_string());
        out.css_class = WARNING.to_string();
        return out;
    };

    // Stale check
    let entry_time = DateTime::from_timestamp(entry.date / 1000, 0);
    let is_stale = match entry_time {
        Some(t) => Utc::now().signed_duration_since(t) > Duration::seconds(cfg.stale_mins * 60),
        None => true,
    };
    if is_stale {
        out.status_line = "NS: STALE".to_string();
        out.tooltip_lines
            .push(format!("CGM data is older than {} minutes", cfg.stale_mins));
        out.css_class = WARNING.to_string();
        return out;
    }

    // Convert mg/dL -> mmol/L if needed
    let bg = if entry.sgv > 20.0 {
        entry.sgv / 18.0
    } else {
        entry.sgv
    };
    let direction = entry.direction.as_deref().unwrap_or("NONE");
    let arrow = arrow_for(direction);

    let (state, description) = if bg < cfg.low_crit {
        (CRITICAL, "low")
    } else if bg < cfg.low_warn {
        (WARNING, "low")
    } else if bg > cfg.high_crit {
        (CRITICAL, "high")
    } else if bg > cfg.high_warn {
        (WARNING, "high")
    } else {
        (NORMAL, "normal")
    };

    out.status_line = format!("{bg:.1} {arrow}");
    out.tooltip_lines
        .push(format!("Current: {bg:.1} mmol/L {arrow}"));
    out.tooltip_lines
        .push(format!("Status: {description} ({state})"));
    out.css_class = state.to_string();

    if is_degradation(prev_state, state) {
        out.notifications.push(Notification {
            message: format!("BG ALERT: {bg:.1} mmol/L ({state})"),
            critical: state == CRITICAL,
        });
    }

    out.cache_state = serde_json::json!({ "state": state });
    out
}

// ============================================================================
// Pump module
// ============================================================================

#[derive(Deserialize)]
struct DeviceStatus {
    created_at: Option<String>,
    #[serde(default)]
    pump: Option<Pump>,
}

#[derive(Deserialize)]
struct Pump {
    reservoir: Option<f64>,
}

#[derive(Deserialize)]
struct Treatment {
    created_at: Option<String>,
}

fn run_pump_module(url: &str, cfg: &PumpConfig, prev_cache: &serde_json::Value) -> ModuleOutput {
    let mut out = ModuleOutput::empty();
    let mut res_state = NORMAL.to_string();
    let mut exp_state = NORMAL.to_string();

    // --- Reservoir ---
    let ds_endpoint = format!("{url}/api/v1/devicestatus.json?count=1");
    let reservoir = http_get_json::<Vec<DeviceStatus>>(&ds_endpoint)
        .ok()
        .and_then(|v| v.into_iter().next())
        .and_then(|ds| ds.pump)
        .and_then(|p| p.reservoir);

    if let Some(res) = reservoir {
        res_state = if res < cfg.res_crit {
            CRITICAL
        } else if res < cfg.res_warn {
            WARNING
        } else {
            NORMAL
        }
        .to_string();

        out.tooltip_lines
            .push(format!("\nReservoir: {res:.1}U ({res_state})"));

        if res <= cfg.res_show {
            let marker = match res_state.as_str() {
                CRITICAL => "!",
                WARNING => "*",
                _ => "",
            };
            out.status_line = format!("{res:.0}U{marker}");
        }
    }

    // --- Pod expiry ---
    let tx_endpoint = format!("{url}/api/v1/treatments.json?eventType=Pod%20Change&count=1");
    let pod_change = http_get_json::<Vec<Treatment>>(&tx_endpoint)
        .ok()
        .and_then(|v| v.into_iter().next())
        .and_then(|t| t.created_at)
        .and_then(|s| parse_time(&s));

    let mut expiry_display = String::new();

    if let Some(change_dt) = pod_change {
        let expiry = change_dt + Duration::seconds(cfg.lifetime_hours * 3600);
        let hours_remaining = (expiry - Utc::now()).num_seconds() as f64 / 3600.0;

        exp_state = if hours_remaining < cfg.expiry_crit_hours as f64 {
            CRITICAL
        } else if hours_remaining < cfg.expiry_warn_hours as f64 {
            WARNING
        } else {
            NORMAL
        }
        .to_string();

        let (display, tooltip) = format_hours(hours_remaining);
        out.tooltip_lines
            .push(format!("Pod expires: {tooltip} ({exp_state})"));

        if exp_state != NORMAL {
            expiry_display = display.clone();
            if !out.status_line.is_empty() {
                out.status_line.push(' ');
            }
            out.status_line.push_str(&display);
        }
    }

    // --- Combine state ---
    out.css_class = if res_state == CRITICAL || exp_state == CRITICAL {
        CRITICAL.to_string()
    } else if res_state == WARNING || exp_state == WARNING {
        WARNING.to_string()
    } else {
        NORMAL.to_string()
    };

    // --- Notifications ---
    let prev_res = prev_cache
        .get("res_state")
        .and_then(|v| v.as_str())
        .unwrap_or(NORMAL);
    let prev_exp = prev_cache
        .get("exp_state")
        .and_then(|v| v.as_str())
        .unwrap_or(NORMAL);

    if is_degradation(prev_res, &res_state) {
        if let Some(res) = reservoir {
            out.notifications.push(Notification {
                message: format!("RESERVOIR ALERT: {res:.1}U ({res_state})"),
                critical: res_state == CRITICAL,
            });
        }
    }

    if is_degradation(prev_exp, &exp_state) && !expiry_display.is_empty() {
        out.notifications.push(Notification {
            message: format!("POD ALERT: Expires in {expiry_display} ({exp_state})"),
            critical: exp_state == CRITICAL,
        });
    }

    out.cache_state = serde_json::json!({
        "res_state": res_state,
        "exp_state": exp_state,
    });
    out
}

// ============================================================================
// Transmitter module
// ============================================================================

#[derive(Deserialize)]
struct Transmitter {
    #[serde(rename = "transmitterStartDate")]
    start_date: Option<String>,
    #[serde(rename = "transmitterEndDate")]
    end_date: Option<String>,
}

#[derive(Deserialize)]
struct DeviceStatusWithTx {
    #[serde(default)]
    transmitter: Option<Transmitter>,
}

fn format_age(start: DateTime<Utc>) -> String {
    let delta = Utc::now() - start;
    let days = delta.num_days();
    let hours = delta.num_hours() % 24;
    format!("{days}d{hours}h")
}

fn run_transmitter_module(url: &str, cfg: &TransmitterConfig, prev_state: &str) -> ModuleOutput {
    let mut out = ModuleOutput::empty();
    let endpoint = format!("{url}/api/v1/devicestatus.json?count=1");

    let tx = match http_get_json::<Vec<DeviceStatusWithTx>>(&endpoint) {
        Ok(v) => v.into_iter().next().and_then(|d| d.transmitter),
        Err(_) => None,
    };

    let Some(tx) = tx else {
        return out;
    };

    let start = tx.start_date.as_deref().and_then(parse_time);
    let end = tx.end_date.as_deref().and_then(parse_time);

    let age_str = start
        .map(format_age)
        .unwrap_or_else(|| "Unknown".to_string());

    let (state, display, tooltip_remaining) = match end {
        Some(end_dt) => {
            let hours_remaining = (end_dt - Utc::now()).num_seconds() as f64 / 3600.0;
            let state = if hours_remaining < cfg.expiry_crit_hours as f64 {
                CRITICAL
            } else if hours_remaining < cfg.expiry_warn_hours as f64 {
                WARNING
            } else {
                NORMAL
            };
            let (display, tooltip) = format_hours(hours_remaining);
            (state, display, Some(tooltip))
        }
        None => (NORMAL, "?".to_string(), None),
    };

    let mut tooltip = format!("Transmitter age: {age_str}");
    if let Some(rem) = &tooltip_remaining {
        tooltip.push_str(&format!(", Expires in: {rem}"));
    }
    out.tooltip_lines.push(tooltip);
    out.tooltip_lines.push(format!("Tx status: {state}"));

    if state != NORMAL {
        out.status_line = format!("Tx:{display}");
        out.css_class = state.to_string();
    }

    if is_degradation(prev_state, state) {
        let rem = tooltip_remaining.unwrap_or_else(|| display.clone());
        out.notifications.push(Notification {
            message: format!("TRANSMITTER ALERT: Expires in {rem} ({state})"),
            critical: state == CRITICAL,
        });
    }

    out.cache_state = serde_json::json!({ "state": state });
    out
}

// ============================================================================
// Cache
// ============================================================================

fn load_cache(path: &str) -> serde_json::Value {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

fn save_cache(path: &str, value: &serde_json::Value) {
    if let Ok(s) = serde_json::to_string(value) {
        let _ = fs::write(path, s);
    }
}

// ============================================================================
// Notifications
// ============================================================================

fn send_notification(n: &Notification) {
    let urgency = if n.critical { "critical" } else { "normal" };
    let _ = Command::new("notify-send")
        .args(["-u", urgency, "CGM Alert", &n.message])
        .status();
}

// ============================================================================
// main
// ============================================================================

fn main() {
    let cli = Cli::parse();
    let cfg = Config::load(cli.config.as_ref());

    let cache = load_cache(&cfg.cache_file_path);

    // --- Run modules ---
    let bg_prev = cache
        .get("bg")
        .and_then(|v| v.get("state"))
        .and_then(|v| v.as_str())
        .unwrap_or(NORMAL);
    let tx_prev = cache
        .get("transmitter")
        .and_then(|v| v.get("state"))
        .and_then(|v| v.as_str())
        .unwrap_or(NORMAL);
    let pump_prev = cache
        .get("pump")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    let outputs = vec![
        run_bg_module(&cli.url, &cfg.bg, bg_prev),
        run_pump_module(&cli.url, &cfg.pump, &pump_prev),
        run_transmitter_module(&cli.url, &cfg.transmitter, tx_prev),
    ];

    // --- Combine outputs ---
    let mut status_line = String::new();
    let mut tooltip_lines: Vec<String> = Vec::new();
    let mut classes: Vec<String> = Vec::new();
    let mut notifications: Vec<Notification> = Vec::new();
    let mut new_cache = serde_json::json!({});

    for (name, out) in ["bg", "pump", "transmitter"].iter().zip(outputs) {
        if !out.status_line.is_empty() {
            if !status_line.is_empty() {
                status_line.push(' ');
            }
            status_line.push_str(&out.status_line);
        }

        tooltip_lines.extend(out.tooltip_lines);

        if out.css_class != NORMAL {
            classes.push(out.css_class);
        }

        notifications.extend(out.notifications);

        new_cache[name] = out.cache_state;
    }

    // Overall class
    let final_class = if classes.iter().any(|c| c == CRITICAL) {
        CRITICAL
    } else if classes.iter().any(|c| c == WARNING) {
        WARNING
    } else if classes.iter().any(|c| c == ERROR) {
        ERROR
    } else {
        NORMAL
    };

    // --- Send notifications ---
    for n in &notifications {
        send_notification(n);
    }

    // --- Save cache ---
    save_cache(&cfg.cache_file_path, &new_cache);

    // --- Emit Waybar JSON ---
    let output = WaybarOutput {
        text: status_line,
        tooltip: tooltip_lines.join("\n"),
        class: final_class.to_string(),
    };

    println!("{}", serde_json::to_string(&output).unwrap());
}
