use chrono::{DateTime, Duration, Utc};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::cmp::max;
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

	/// Use mmol/L units (instead of mg/dL default)
	#[arg(long = "mmol")]
	use_mmol_units: bool,
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
}

#[derive(Deserialize, Debug, Clone)]
#[serde(default)]
struct BgConfig {
	low_warn: f64,
	high_warn: f64,
	low_crit: f64,
	high_crit: f64,
	stale_mins: i64,
	use_mmol_units: bool,
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

impl Default for Config {
	fn default() -> Self {
		Self {
			cache_file_path: "/tmp/waybar_nightscout_state.json".to_string(),
			bg: BgConfig::default(),
			pump: PumpConfig::default(),
		}
	}
}

impl Default for BgConfig {
	fn default() -> Self {
		Self {
			low_warn: 75.0,
			high_warn: 180.0,
			low_crit: 60.0,
			high_crit: 250.0,
			stale_mins: 15,
			use_mmol_units: false,
		}
	}
}

impl Default for PumpConfig {
	fn default() -> Self {
		Self {
			res_show: 50.0,
			res_warn: 40.0,
			res_crit: 30.0,
			expiry_warn_hours: 24,
			expiry_crit_hours: 2,
			lifetime_hours: 72,
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
			},
		}
	}
}

// ============================================================================
// Shared types
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
enum Severity {
	Info,
	Warning,
	Critical,
}

impl From<Severity> for &str {
	fn from(val: Severity) -> Self {
		match val {
			Severity::Critical => "crit",
			Severity::Warning => "warn",
			Severity::Info => "info",
		}
	}
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
enum ModuleError {
	Http,
	CacheParse,
	StaleOrMissingData,
}

#[derive(Clone)]
/// Output contributed by a single module.
struct ModuleOutput {
	status_line: String,
	tooltip_lines: Vec<String>,
	severity: Severity,
	notifications: Vec<Notification>,
	error: Option<ModuleError>,
}

impl ModuleOutput {
	fn empty() -> Self {
		Self {
			status_line: String::new(),
			tooltip_lines: Vec::new(),
			severity: Severity::Info,
			notifications: Vec::new(),
			error: None,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NotificationUrgency {
	Low,
	Normal,
	Critical,
}

impl From<NotificationUrgency> for &str {
	fn from(val: NotificationUrgency) -> Self {
		match val {
			NotificationUrgency::Low => "low",
			NotificationUrgency::Normal => "normal",
			NotificationUrgency::Critical => "critical",
		}
	}
}

impl From<Severity> for NotificationUrgency {
	fn from(value: Severity) -> Self {
		match value {
			Severity::Critical => NotificationUrgency::Critical,
			Severity::Warning => NotificationUrgency::Normal,
			Severity::Info => NotificationUrgency::Low,
		}
	}
}

#[derive(Clone)]
struct Notification {
	message: String,
	urgency: NotificationUrgency,
}

#[derive(Serialize)]
struct WaybarOutput {
	text: String,
	tooltip: String,
	class: String,
}

// ============================================================================
// HTTP helpers
// ============================================================================

fn http_get_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, String> {
	ureq::AgentBuilder::new()
		.timeout(std::time::Duration::from_secs(10))
		.build()
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
	date: f64,
	#[serde(default)]
	direction: Option<String>,
}

type BgCache = Result<f64, ModuleError>;

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

fn classify_bg(cfg: &BgConfig, bg: f64) -> Severity {
	if bg < convert_units(cfg.low_crit, cfg.use_mmol_units) {
		Severity::Critical
	} else if bg < convert_units(cfg.low_warn, cfg.use_mmol_units) {
		Severity::Warning
	} else if bg > convert_units(cfg.high_crit, cfg.use_mmol_units) {
		Severity::Critical
	} else if bg > convert_units(cfg.high_warn, cfg.use_mmol_units) {
		Severity::Warning
	} else {
		Severity::Info
	}
}

fn convert_units(bg_mgdl: f64, use_mmol: bool) -> f64 {
	if use_mmol { bg_mgdl / 18.0 } else { bg_mgdl }
}

fn run_bg_module(url: &str, cfg: &BgConfig, cached: BgCache) -> (BgCache, ModuleOutput) {
	let mut out = ModuleOutput::empty();
	let endpoint = format!("{url}/api/v1/entries.json?count=1");

	let entries: Vec<Entry> = match http_get_json(&endpoint) {
		Ok(v) => v,
		Err(_) => {
			let err = ModuleError::Http;
			let msg = "Failed to fetch data from NightScout".to_string();
			out.status_line = "NS: API ERR".to_string();
			out.tooltip_lines.push(msg.clone());
			out.severity = Severity::Info;
			out.error = Some(err);
			out.notifications.push(Notification {
				message: msg,
				urgency: NotificationUrgency::Critical,
			});
			return (BgCache::Err(err), out);
		},
	};

	let Some(entry) = entries.into_iter().next() else {
		let err = ModuleError::StaleOrMissingData;
		let msg = "No entries found in NightScout response".to_string();
		out.status_line = "NS: NO DATA".to_string();
		out.tooltip_lines.push(msg.clone());
		out.severity = Severity::Info;
		out.error = Some(err);
		out.notifications.push(Notification {
			message: msg,
			urgency: NotificationUrgency::Critical,
		});
		return (BgCache::Err(err), out);
	};

	// Stale check
	let entry_time = DateTime::from_timestamp(entry.date.round() as i64 / 1000, 0);
	let is_stale = match entry_time {
		Some(t) => Utc::now().signed_duration_since(t) > Duration::seconds(cfg.stale_mins * 60),
		None => true,
	};
	if is_stale {
		let err = ModuleError::StaleOrMissingData;
		let msg = format!("CGM data is older than {} minutes", cfg.stale_mins);
		out.status_line = "NS: STALE".to_string();
		out.tooltip_lines.push(msg.clone());
		out.severity = Severity::Info;
		out.error = Some(err);
		out.notifications.push(Notification {
			message: msg,
			urgency: NotificationUrgency::Critical,
		});

		return (BgCache::Err(err), out);
	}

	let bg = convert_units(entry.sgv, cfg.use_mmol_units);
	let direction = entry.direction.as_deref().unwrap_or("NONE");
	let arrow = arrow_for(direction);

	let severity = classify_bg(cfg, bg);
	let description = if bg > convert_units(cfg.high_warn, cfg.use_mmol_units) {
		"high"
	} else if bg < convert_units(cfg.low_warn, cfg.use_mmol_units) {
		"low"
	} else {
		"ok"
	};

	out.status_line = format!("{bg:.1} {arrow}");
	out.tooltip_lines.push(if cfg.use_mmol_units {
		format!("Current: {bg:.1} mmol/L {arrow}")
	} else {
		format!("Current: {bg:.0} mmol/L {arrow}")
	});
	out.tooltip_lines.push(format!(
		"Status: {description} ({})",
		<Severity as Into<&str>>::into(severity)
	));
	out.severity = severity;

	if match cached {
		BgCache::Ok(cache_bg) => classify_bg(cfg, cache_bg) != severity,
		BgCache::Err(_) => true,
	} {
		out.notifications.push(Notification {
			message: if cfg.use_mmol_units {
				format!(
					"BG ALERT: {bg:.1} mmol/L ({})",
					<Severity as Into<&str>>::into(severity)
				)
			} else {
				format!(
					"BG ALERT: {bg:.0} mmol/L ({})",
					<Severity as Into<&str>>::into(severity)
				)
			},
			urgency: out.severity.into(),
		});
	}

	(BgCache::Ok(bg), out)
}

// ============================================================================
// Pump module
// ============================================================================

#[derive(Deserialize)]
struct DeviceStatus {
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

type PumpCache = Result<(Severity, Severity), ModuleError>;

fn run_pump_module(url: &str, cfg: &PumpConfig, cached: PumpCache) -> (PumpCache, ModuleOutput) {
	let mut out = ModuleOutput::empty();
	let mut res_state = Severity::Info;
	let mut exp_state = Severity::Info;

	// --- Reservoir ---
	let ds_endpoint = format!("{url}/api/v1/devicestatus.json?count=1");
	let reservoir = http_get_json::<Vec<DeviceStatus>>(&ds_endpoint)
		.ok()
		.and_then(|v| v.into_iter().next())
		.and_then(|ds| ds.pump)
		.and_then(|p| p.reservoir);

	if let Some(res) = reservoir {
		res_state = if res < cfg.res_crit {
			Severity::Critical
		} else if res < cfg.res_warn {
			Severity::Warning
		} else {
			Severity::Info
		};

		out.tooltip_lines.push(format!(
			"\nReservoir: {res:.1}U ({})",
			<Severity as Into<&str>>::into(res_state)
		));

		if res <= cfg.res_show {
			let marker = match res_state {
				Severity::Critical => "!",
				Severity::Warning => "*",
				_ => "",
			};
			out.status_line = format!("{res:.0}U{marker}");
		}
	}

	// --- Pod expiry ---
	let tx_endpoint = format!("{url}/api/v1/treatments.json?find[eventType]=Pod+Change&count=1");
	let pod_change = http_get_json::<Vec<Treatment>>(&tx_endpoint)
		.ok()
		.and_then(|v| v.into_iter().next())
		.and_then(|t| t.created_at)
		.and_then(|s| parse_time(&s));

	let mut expiry_display = String::new();

	if let Some(change_dt) = pod_change {
		let expiry = change_dt + Duration::seconds(cfg.lifetime_hours * 3600);
		let hours_remaining = (expiry - Utc::now()).num_seconds() as f64 / 3600.0;
		println!(
			"remain {} exp {} changed {}",
			hours_remaining, expiry, change_dt
		);

		exp_state = if hours_remaining < cfg.expiry_crit_hours as f64 {
			Severity::Critical
		} else if hours_remaining < cfg.expiry_warn_hours as f64 {
			Severity::Warning
		} else {
			Severity::Info
		};

		let (display, tooltip) = format_hours(hours_remaining);
		out.tooltip_lines.push(format!(
			"Pod expires: {tooltip} ({})",
			<Severity as Into<&str>>::into(exp_state)
		));

		if exp_state != Severity::Info {
			expiry_display = display.clone();
			if !out.status_line.is_empty() {
				out.status_line.push(' ');
			}
			out.status_line.push_str(&display);
		}
	}

	// --- Combine state ---
	out.severity = max(exp_state, res_state);

	// --- Notifications ---
	let (prev_res, prev_exp) = match cached {
		PumpCache::Ok((r, x)) => (Some(r), Some(x)),
		PumpCache::Err(_) => (None, None),
	};

	if Some(res_state) != prev_res
		&& let Some(res) = reservoir
	{
		out.notifications.push(Notification {
			message: format!(
				"RESERVOIR ALERT: {res:.1}U ({})",
				<Severity as Into<&str>>::into(exp_state)
			),
			urgency: res_state.into(),
		});
	}

	if Some(exp_state) != prev_exp && !expiry_display.is_empty() {
		out.notifications.push(Notification {
			message: format!(
				"POD ALERT: Expires in {expiry_display} ({})",
				<Severity as Into<&str>>::into(exp_state)
			),
			urgency: exp_state.into(),
		});
	}

	(PumpCache::Ok((res_state, exp_state)), out)
}

// ============================================================================
// Cache
// ============================================================================

#[derive(Serialize, Deserialize)]
struct Cache {
	bg: BgCache,
	pump: PumpCache,
}

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
	let _ = Command::new("notify-send")
		.args(["-u", n.urgency.into(), "CGM Alert", &n.message])
		.status();
}

// ============================================================================
// main
// ============================================================================

fn main() {
	let cli = Cli::parse();
	let mut cfg = Config::load(cli.config.as_ref());
	cfg.bg.use_mmol_units |= cli.use_mmol_units;

	let cache: Cache = serde_json::from_value(load_cache(&cfg.cache_file_path)).unwrap_or(Cache {
		bg: Err(ModuleError::CacheParse),
		pump: Err(ModuleError::CacheParse),
	});

	// --- Run modules ---
	let bg_result = run_bg_module(&cli.url, &cfg.bg, cache.bg);
	let pump_result = run_pump_module(&cli.url, &cfg.pump, cache.pump);

	// --- Combine outputs ---
	let outputs = [bg_result.1, pump_result.1];

	let status_line = outputs
		.clone()
		.map(|r| r.status_line)
		.iter()
		.filter(|l| l != &"")
		.fold("".to_string(), |a, b| format!("{} {}", a.trim(), b.trim()));

	let tooltip_lines = outputs.clone().map(|r| r.tooltip_lines).concat();

	let overall_severity = outputs
		.clone()
		.map(|r| r.severity)
		.into_iter()
		.reduce(max)
		.unwrap_or(Severity::Info);

	let notifications = outputs.clone().map(|r| r.notifications).concat();

	let had_errors = outputs.map(|r| r.error.is_some()).iter().any(|x| *x);

	// --- Send notifications ---
	for n in &notifications {
		send_notification(n);
	}

	// --- Save cache ---
	save_cache(
		&cfg.cache_file_path,
		&serde_json::to_value(Cache {
			bg: bg_result.0,
			pump: pump_result.0,
		})
		.unwrap(),
	);

	// --- Emit Waybar JSON ---
	let output = WaybarOutput {
		text: status_line,
		tooltip: tooltip_lines.join("\n"),
		class: if had_errors {
			"error".to_string()
		} else {
			<Severity as Into<&str>>::into(overall_severity).to_string()
		},
	};

	println!("{}", serde_json::to_string(&output).unwrap());
}
