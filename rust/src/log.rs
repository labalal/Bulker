use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Hand-rolled file logger: no tracing/flexi_logger dependency, just lines
/// of "<UTC timestamp> [<LEVEL>] <msg>" appended to app.log (same format the
/// Python version produced) plus a stderr echo. Kept behind a Mutex so
/// concurrent worker tasks interleave cleanly.
struct FileLogger {
    file: Mutex<Option<File>>,
}

static LOGGER: OnceLock<FileLogger> = std::sync::OnceLock::new();

use std::sync::OnceLock;

pub fn init(path: &str) {
    let file = OpenOptions::new().create(true).append(true).open(path).ok();
    let _ = LOGGER.set(FileLogger { file: Mutex::new(file) });
}

/// Days-since-epoch -> (y, m, d), Howard Hinnant's civil_from_days.
/// Months/days math done directly, no chrono dependency.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn utc_timestamp() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as i64;
    let millis = now.subsec_millis();
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02},{millis:03}",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

pub fn log(level: &str, msg: &str) {
    let line = format!("{} [{}] {}", utc_timestamp(), level, msg);
    eprintln!("{line}");
    if let Some(logger) = LOGGER.get() {
        if let Ok(mut guard) = logger.file.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = writeln!(file, "{line}");
                let _ = file.flush();
            }
        }
    }
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log::log("INFO", &format!($($arg)*)) };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log::log("WARNING", &format!($($arg)*)) };
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log::log("ERROR", &format!($($arg)*)) };
}

/// Returns the last `n` lines of the log file (for the UI's raw-logs panel).
pub fn read_last_lines(path: &str, n: usize) -> String {
    let Ok(file) = File::open(Path::new(path)) else {
        return String::new();
    };
    let mut lines = Vec::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        lines.push(line);
    }
    let start = lines.len().saturating_sub(n);
    let mut out = String::new();
    for line in &lines[start..] {
        if !line.is_empty() {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}
