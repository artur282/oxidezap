//! Performance and battery-saver policy.
//!
//! Everything in this module is pure except [`detect_power_state`], which only
//! reads `/sys/class/power_supply`. Keeping the decision table here lets the
//! settings window, the GPUI pump, and the GTK/WebKit thread agree on the same
//! values without a display, a battery, or a running WebKit process.
//!
//! The battery saver is deliberately conservative: it changes animation,
//! polling, and synchronization cadence, but it never disables notifications,
//! downloads, calls, the app lock, security checks, or the WhatsApp login.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::MemorySettings;

/// A hidden account is released after at most this delay while the battery
/// saver is active. Only applies when low-memory mode is already enabled.
pub const BATTERY_SUSPEND_MINUTES: u32 = 15;

/// How often the GPUI model re-reads the power supply. The files are tiny and
/// the value changes rarely, so a slow poll costs less than an inotify watch.
pub const POWER_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Directory exposed by the kernel. Reads are best effort; a missing directory
/// simply means "unknown", which resolves to normal performance.
pub const POWER_SUPPLY_ROOT: &str = "/sys/class/power_supply";

/// User selection persisted in `settings.json` under `performance.mode`.
///
/// `Normal` is the default so upgrading from a build without this feature
/// never changes behavior on its own. `Automatic` opts into the saver only
/// while the machine reports battery power, and `BatterySaver` forces it.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PerformanceMode {
    Automatic,
    #[default]
    Normal,
    BatterySaver,
}

impl PerformanceMode {
    pub const ALL: [PerformanceMode; 3] = [
        PerformanceMode::Automatic,
        PerformanceMode::Normal,
        PerformanceMode::BatterySaver,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PerformanceMode::Automatic => "Automatic",
            PerformanceMode::Normal => "Normal",
            PerformanceMode::BatterySaver => "Battery saver",
        }
    }

    /// Short description shown under the mode buttons.
    pub fn description(self) -> &'static str {
        match self {
            PerformanceMode::Automatic => "Use battery saver only while on battery power",
            PerformanceMode::Normal => "Never reduce animation or polling",
            PerformanceMode::BatterySaver => "Always reduce animation and polling",
        }
    }

    /// Controls the element ids used by the settings window and the value
    /// written to `settings.json`.
    pub fn slug(self) -> &'static str {
        match self {
            PerformanceMode::Automatic => "automatic",
            PerformanceMode::Normal => "normal",
            PerformanceMode::BatterySaver => "battery-saver",
        }
    }

    /// Unknown values fall back to `Normal` instead of discarding the whole
    /// settings file.
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "automatic" | "auto" => PerformanceMode::Automatic,
            "battery_saver" | "battery" | "saver" => PerformanceMode::BatterySaver,
            _ => PerformanceMode::Normal,
        }
    }
}

impl<'de> Deserialize<'de> for PerformanceMode {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(Self::parse(&value))
    }
}

/// Persisted performance preferences. The `#[serde(default)]` attribute keeps
/// settings files written before this feature readable: they simply get
/// `Normal`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PerformanceSettings {
    pub mode: PerformanceMode,
}

impl PerformanceSettings {
    /// Resolves the persisted mode against the current power source. This is
    /// the single decision point used by the application model.
    pub fn resolve(&self, power: PowerState) -> PerformanceStatus {
        let (battery_saver, reason) = match self.mode {
            PerformanceMode::Normal => (false, PerformanceReason::NormalMode),
            PerformanceMode::BatterySaver => (true, PerformanceReason::ManualBatterySaver),
            PerformanceMode::Automatic => match power {
                PowerState::OnBattery => (true, PerformanceReason::AutomaticOnBattery),
                PowerState::OnAc => (false, PerformanceReason::AutomaticOnAc),
                PowerState::Unknown => (false, PerformanceReason::AutomaticUnknown),
            },
        };
        PerformanceStatus {
            battery_saver,
            reason,
            power,
        }
    }
}

/// Kernel power source. `Unknown` is the graceful fallback for a system with
/// no readable `/sys/class/power_supply`, a container, or a permission error.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PowerState {
    OnBattery,
    OnAc,
    #[default]
    Unknown,
}

impl PowerState {
    pub fn label(self) -> &'static str {
        match self {
            PowerState::OnBattery => "On battery",
            PowerState::OnAc => "On AC power",
            PowerState::Unknown => "Battery state unavailable",
        }
    }
}

/// Why the resolved mode is what it is. Kept separate from the boolean so the
/// settings window can explain automatic decisions and their fallbacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PerformanceReason {
    NormalMode,
    ManualBatterySaver,
    AutomaticOnBattery,
    AutomaticOnAc,
    AutomaticUnknown,
}

/// Effective performance state after combining the persisted mode with the
/// detected power source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PerformanceStatus {
    pub battery_saver: bool,
    pub reason: PerformanceReason,
    pub power: PowerState,
}

impl PerformanceStatus {
    pub fn cadence(self) -> PerformanceCadence {
        PerformanceCadence::for_battery_saver(self.battery_saver)
    }

    pub fn status_text(self) -> &'static str {
        match self.reason {
            PerformanceReason::NormalMode => "Normal performance. Battery saver is off.",
            PerformanceReason::ManualBatterySaver => {
                "Battery saver is on because it was selected manually."
            }
            PerformanceReason::AutomaticOnBattery => {
                "Battery saver is on automatically because the device is on battery."
            }
            PerformanceReason::AutomaticOnAc => {
                "Normal performance. Automatic mode stays off while on AC power."
            }
            PerformanceReason::AutomaticUnknown => {
                "Normal performance. No battery was detected, so automatic mode stays off."
            }
        }
    }
}

/// Timer and polling intervals used by the native shell and WebKit.
///
/// Lower cadence is expressed as a longer duration: the overlay is synchronized
/// less often, the connection script runs less often, and expensive checks run
/// farther apart. The values are policy, not measurements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PerformanceCadence {
    pub pump: Duration,
    pub overlay_sync: Duration,
    pub connection_report: Duration,
    pub dnd_check: Duration,
    pub scale_check: Duration,
    pub memory_sweep: Duration,
}

impl PerformanceCadence {
    pub const NORMAL: PerformanceCadence = PerformanceCadence {
        pump: Duration::from_millis(100),
        overlay_sync: Duration::from_millis(100),
        connection_report: Duration::from_secs(2),
        dnd_check: Duration::from_secs(1),
        scale_check: Duration::from_secs(1),
        memory_sweep: Duration::from_secs(60),
    };

    pub const BATTERY: PerformanceCadence = PerformanceCadence {
        pump: Duration::from_millis(200),
        overlay_sync: Duration::from_millis(250),
        connection_report: Duration::from_secs(5),
        dnd_check: Duration::from_secs(5),
        scale_check: Duration::from_secs(5),
        memory_sweep: Duration::from_secs(30),
    };

    pub const fn for_battery_saver(battery_saver: bool) -> Self {
        if battery_saver {
            Self::BATTERY
        } else {
            Self::NORMAL
        }
    }
}

/// The low-memory delay a battery-saver sweep should use.
///
/// This deliberately does not enable low-memory mode: the user's master switch
/// and every safety guard in the existing policy stay in charge. It only
/// shortens an already enabled delay so hidden accounts are released sooner.
pub fn effective_memory(memory: &MemorySettings, battery_saver: bool) -> MemorySettings {
    let mut effective = memory.clone();
    if battery_saver && effective.suspend_hidden {
        effective.suspend_after_minutes =
            effective.suspend_after_minutes.min(BATTERY_SUSPEND_MINUTES);
    }
    effective
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerSupplyKind {
    Battery,
    Mains,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerSupplyStatus {
    Charging,
    Discharging,
    Full,
    NotCharging,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerSupply {
    pub kind: PowerSupplyKind,
    pub status: PowerSupplyStatus,
}

/// Parses the two sysfs attributes this module needs. Unknown values are kept
/// as `Other`/`Unknown` so a new kernel attribute cannot break detection.
pub fn parse_power_supply(kind: &str, status: &str) -> PowerSupply {
    let kind = match kind.trim() {
        "Battery" => PowerSupplyKind::Battery,
        "Mains" | "USB" | "AC" => PowerSupplyKind::Mains,
        _ => PowerSupplyKind::Other,
    };
    let status = match status.trim() {
        "Charging" => PowerSupplyStatus::Charging,
        "Discharging" => PowerSupplyStatus::Discharging,
        "Full" => PowerSupplyStatus::Full,
        "Not charging" => PowerSupplyStatus::NotCharging,
        _ => PowerSupplyStatus::Unknown,
    };
    PowerSupply { kind, status }
}

/// Resolves a list of kernel supplies. A discharging battery wins over an
/// online mains adapter, which is what makes a partially charged laptop report
/// `OnBattery`. A desktop with only a mains supply reports `OnAc`; no supplies
/// at all remains `Unknown`.
pub fn power_state(supplies: &[PowerSupply]) -> PowerState {
    let has_battery = supplies
        .iter()
        .any(|supply| supply.kind == PowerSupplyKind::Battery);
    if supplies.iter().any(|supply| {
        supply.kind == PowerSupplyKind::Battery && supply.status == PowerSupplyStatus::Discharging
    }) {
        return PowerState::OnBattery;
    }
    if supplies
        .iter()
        .any(|supply| supply.kind == PowerSupplyKind::Mains)
    {
        return PowerState::OnAc;
    }
    if has_battery
        && supplies.iter().any(|supply| {
            supply.kind == PowerSupplyKind::Battery
                && matches!(
                    supply.status,
                    PowerSupplyStatus::Charging
                        | PowerSupplyStatus::Full
                        | PowerSupplyStatus::NotCharging
                )
        })
    {
        return PowerState::OnAc;
    }
    PowerState::Unknown
}

/// Reads the power source from the live sysfs tree. Read-only and failure
/// tolerant: an unreadable node is skipped and the final state falls back to
/// `Unknown`, which resolves to normal performance.
pub fn detect_power_state() -> PowerState {
    detect_power_state_in(Path::new(POWER_SUPPLY_ROOT))
}

/// Same as [`detect_power_state`] with an explicit root, so tests never depend
/// on the host's hardware.
pub fn detect_power_state_in(root: &Path) -> PowerState {
    let Ok(entries) = std::fs::read_dir(root) else {
        return PowerState::Unknown;
    };
    let mut supplies = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(supply) = read_supply(&path) {
            supplies.push(supply);
        }
    }
    power_state(&supplies)
}

fn read_supply(path: &Path) -> Option<PowerSupply> {
    let kind = std::fs::read_to_string(path.join("type")).ok()?;
    let status = std::fs::read_to_string(path.join("status")).unwrap_or_default();
    let supply = parse_power_supply(&kind, &status);
    (supply.kind != PowerSupplyKind::Other).then_some(supply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn battery(status: PowerSupplyStatus) -> PowerSupply {
        PowerSupply {
            kind: PowerSupplyKind::Battery,
            status,
        }
    }

    fn mains(status: PowerSupplyStatus) -> PowerSupply {
        PowerSupply {
            kind: PowerSupplyKind::Mains,
            status,
        }
    }

    #[test]
    fn normal_mode_never_enables_the_saver() {
        for power in [PowerState::OnBattery, PowerState::OnAc, PowerState::Unknown] {
            let status = PerformanceSettings {
                mode: PerformanceMode::Normal,
            }
            .resolve(power);
            assert!(!status.battery_saver);
            assert_eq!(status.reason, PerformanceReason::NormalMode);
        }
    }

    #[test]
    fn battery_saver_mode_always_enables_the_saver() {
        for power in [PowerState::OnBattery, PowerState::OnAc, PowerState::Unknown] {
            let status = PerformanceSettings {
                mode: PerformanceMode::BatterySaver,
            }
            .resolve(power);
            assert!(status.battery_saver);
            assert_eq!(status.reason, PerformanceReason::ManualBatterySaver);
        }
    }

    #[test]
    fn automatic_mode_follows_the_power_source() {
        let settings = PerformanceSettings {
            mode: PerformanceMode::Automatic,
        };

        let on_battery = settings.resolve(PowerState::OnBattery);
        assert!(on_battery.battery_saver);
        assert_eq!(on_battery.reason, PerformanceReason::AutomaticOnBattery);

        let on_ac = settings.resolve(PowerState::OnAc);
        assert!(!on_ac.battery_saver);
        assert_eq!(on_ac.reason, PerformanceReason::AutomaticOnAc);

        let unknown = settings.resolve(PowerState::Unknown);
        assert!(!unknown.battery_saver);
        assert_eq!(unknown.reason, PerformanceReason::AutomaticUnknown);
    }

    #[test]
    fn status_text_explains_every_resolution() {
        assert!(
            PerformanceStatus {
                battery_saver: false,
                reason: PerformanceReason::NormalMode,
                power: PowerState::OnAc,
            }
            .status_text()
            .contains("Battery saver is off")
        );
        assert!(
            PerformanceStatus {
                battery_saver: true,
                reason: PerformanceReason::AutomaticOnBattery,
                power: PowerState::OnBattery,
            }
            .status_text()
            .contains("on battery")
        );
        assert!(
            PerformanceStatus {
                battery_saver: false,
                reason: PerformanceReason::AutomaticUnknown,
                power: PowerState::Unknown,
            }
            .status_text()
            .contains("No battery was detected")
        );
    }

    #[test]
    fn unknown_persisted_modes_fall_back_to_normal() {
        assert_eq!(
            serde_json::from_str::<PerformanceSettings>(r#"{"mode":"turbo"}"#)
                .expect("unknown mode should still parse")
                .mode,
            PerformanceMode::Normal
        );
        assert_eq!(
            PerformanceMode::parse("battery-saver"),
            PerformanceMode::BatterySaver
        );
        assert_eq!(PerformanceMode::parse("AUTO"), PerformanceMode::Automatic);
    }

    #[test]
    fn settings_files_without_performance_default_to_normal() {
        let settings: PerformanceSettings =
            serde_json::from_str("{}").expect("older settings must keep parsing");
        assert_eq!(settings.mode, PerformanceMode::Normal);

        let full: crate::settings::Settings =
            serde_json::from_str(r#"{"version":1}"#).expect("an older settings file must load");
        assert_eq!(full.performance.mode, PerformanceMode::Normal);
    }

    #[test]
    fn every_mode_round_trips_through_json() {
        for mode in PerformanceMode::ALL {
            let encoded =
                serde_json::to_string(&PerformanceSettings { mode }).expect("serialize mode");
            let decoded: PerformanceSettings =
                serde_json::from_str(&encoded).expect("deserialize mode");
            assert_eq!(decoded.mode, mode);
        }
    }

    #[test]
    fn battery_cadence_is_lower_but_keeps_critical_work() {
        let normal = PerformanceCadence::NORMAL;
        let battery = PerformanceCadence::BATTERY;
        assert!(battery.pump > normal.pump);
        assert!(battery.overlay_sync > normal.overlay_sync);
        assert!(battery.connection_report > normal.connection_report);
        assert!(battery.dnd_check > normal.dnd_check);
        assert!(battery.scale_check > normal.scale_check);
        assert!(battery.memory_sweep < normal.memory_sweep);
        assert_eq!(
            PerformanceCadence::for_battery_saver(false),
            PerformanceCadence::NORMAL
        );
        assert_eq!(
            PerformanceCadence::for_battery_saver(true),
            PerformanceCadence::BATTERY
        );
    }

    #[test]
    fn battery_cadence_values_are_the_documented_ones() {
        let cadence = PerformanceCadence::BATTERY;
        assert_eq!(cadence.pump, Duration::from_millis(200));
        assert_eq!(cadence.overlay_sync, Duration::from_millis(250));
        assert_eq!(cadence.connection_report, Duration::from_secs(5));
        assert_eq!(cadence.dnd_check, Duration::from_secs(5));
        assert_eq!(cadence.scale_check, Duration::from_secs(5));
        assert_eq!(cadence.memory_sweep, Duration::from_secs(30));
    }

    #[test]
    fn effective_memory_shortens_an_enabled_delay_only() {
        let enabled = MemorySettings {
            suspend_hidden: true,
            suspend_after_minutes: 60,
            keep_active_account: false,
        };
        let effective = effective_memory(&enabled, true);
        assert!(effective.suspend_hidden);
        assert_eq!(effective.suspend_after_minutes, BATTERY_SUSPEND_MINUTES);
        assert!(!effective.keep_active_account);

        assert_eq!(effective_memory(&enabled, false), enabled);

        let disabled = MemorySettings {
            suspend_hidden: false,
            suspend_after_minutes: 60,
            keep_active_account: true,
        };
        assert_eq!(effective_memory(&disabled, true), disabled);
    }

    #[test]
    fn effective_memory_never_lengthens_a_short_delay() {
        let short = MemorySettings {
            suspend_hidden: true,
            suspend_after_minutes: 5,
            keep_active_account: true,
        };
        assert_eq!(effective_memory(&short, true).suspend_after_minutes, 5);
    }

    #[test]
    fn parsing_handles_the_kernel_attribute_spellings() {
        assert_eq!(
            parse_power_supply("Battery\n", "Discharging\n"),
            battery(PowerSupplyStatus::Discharging)
        );
        assert_eq!(
            parse_power_supply("Mains", "Not charging"),
            mains(PowerSupplyStatus::NotCharging)
        );
        assert_eq!(
            parse_power_supply("USB", "Charging"),
            mains(PowerSupplyStatus::Charging)
        );
        assert_eq!(
            parse_power_supply("UPS", "Full").kind,
            PowerSupplyKind::Other
        );
    }

    #[test]
    fn a_discharging_battery_wins_over_an_online_adapter() {
        assert_eq!(
            power_state(&[
                battery(PowerSupplyStatus::Discharging),
                mains(PowerSupplyStatus::Charging),
            ]),
            PowerState::OnBattery
        );
    }

    #[test]
    fn charging_and_full_batteries_report_ac_power() {
        assert_eq!(
            power_state(&[battery(PowerSupplyStatus::Charging)]),
            PowerState::OnAc
        );
        assert_eq!(
            power_state(&[battery(PowerSupplyStatus::Full)]),
            PowerState::OnAc
        );
        assert_eq!(
            power_state(&[mains(PowerSupplyStatus::Unknown)]),
            PowerState::OnAc
        );
    }

    #[test]
    fn missing_or_unreadable_supplies_report_unknown() {
        assert_eq!(power_state(&[]), PowerState::Unknown);
        assert_eq!(
            power_state(&[battery(PowerSupplyStatus::Unknown)]),
            PowerState::Unknown
        );
        assert_eq!(
            detect_power_state_in(Path::new("/definitely/not/a/power/supply")),
            PowerState::Unknown
        );
    }

    #[test]
    fn detection_reads_type_and_status_from_sysfs_layout() {
        let root = std::env::temp_dir().join(format!(
            "oxidezap-power-supply-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ));
        let battery_dir = root.join("BAT0");
        let mains_dir = root.join("AC");
        std::fs::create_dir_all(&battery_dir).expect("create battery directory");
        std::fs::create_dir_all(&mains_dir).expect("create mains directory");
        std::fs::write(battery_dir.join("type"), "Battery\n").expect("write battery type");
        std::fs::write(battery_dir.join("status"), "Discharging\n").expect("write battery status");
        std::fs::write(mains_dir.join("type"), "Mains\n").expect("write mains type");
        std::fs::write(mains_dir.join("status"), "Unknown\n").expect("write mains status");

        assert_eq!(detect_power_state_in(&root), PowerState::OnBattery);

        std::fs::write(battery_dir.join("status"), "Charging\n").expect("write charging status");
        assert_eq!(detect_power_state_in(&root), PowerState::OnAc);

        std::fs::remove_dir_all(&root).expect("clean up the temporary power supply");
    }
}
