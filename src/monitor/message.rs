use log::{debug, error, info, warn};
use notify::{EventKind, RecursiveMode};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use super::watcher::FileProcessor;
use crate::clipboard;
use crate::config::Config;
use crate::ipc;
use crate::parser;
use crate::permissions;
use crate::quick_fill;

static LAST_PROCESSED_ROWID: Mutex<i64> = Mutex::new(0);
const CATCH_UP_POLL_INTERVAL: Duration = Duration::from_secs(2);
const CATCH_UP_POLL_WINDOW: Duration = Duration::from_secs(60);
const FILE_EVENT_SCAN_DEBOUNCE: Duration = Duration::from_millis(350);

struct MessageProcessorState {
    catch_up_until: Mutex<Option<Instant>>,
    catch_up_task_running: AtomicBool,
    scan_lock: Mutex<()>,
    last_file_event_scan_at: Mutex<Option<Instant>>,
}

#[derive(Clone)]
pub struct MessageProcessor {
    state: Arc<MessageProcessorState>,
}

impl MessageProcessor {
    pub fn new() -> Self {
        if let Ok(rowid) = Self::get_latest_message_rowid() {
            let mut last_processed = LAST_PROCESSED_ROWID.lock().unwrap();
            *last_processed = rowid;
            info!("Initialized last processed ROWID to {}", rowid);
        }

        Self {
            state: Arc::new(MessageProcessorState {
                catch_up_until: Mutex::new(None),
                catch_up_task_running: AtomicBool::new(false),
                scan_lock: Mutex::new(()),
                last_file_event_scan_at: Mutex::new(None),
            }),
        }
    }

    // 获取数据库中最新的消息ROWID
    fn get_latest_message_rowid() -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
        let home_dir = env::var("HOME")?;
        let db_path = PathBuf::from(&home_dir).join("Library/Messages/chat.db");

        let output = Self::run_sqlite_query(&db_path, "SELECT MAX(ROWID) FROM message;")?;

        if output.status.success() {
            let output_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !output_str.is_empty() {
                return Ok(output_str.parse()?);
            }
        }

        Ok(0)
    }

    fn is_relevant_event(event_kind: &EventKind) -> bool {
        matches!(
            event_kind,
            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Any
        )
    }

    fn is_relevant_messages_path(path: &Path) -> bool {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();

        matches!(file_name, "chat.db" | "chat.db-wal" | "chat.db-shm")
            || (path
                .components()
                .any(|component| component.as_os_str() == std::ffi::OsStr::new("NickNameCache"))
                && path.extension().and_then(|ext| ext.to_str()) == Some("db"))
    }

    fn is_noisy_messages_path(path: &Path) -> bool {
        path.file_name().and_then(|name| name.to_str()) == Some("chat.db-shm")
    }

    fn should_scan_immediately_for_file_event(&self, path: &Path) -> bool {
        if Self::is_noisy_messages_path(path) {
            debug!(
                "Skipping immediate scan for noisy Messages path: {}",
                path.display()
            );
            return false;
        }

        let now = Instant::now();
        let mut last_scan_at = self.state.last_file_event_scan_at.lock().unwrap();

        if let Some(last) = *last_scan_at {
            if now.duration_since(last) < FILE_EVENT_SCAN_DEBOUNCE {
                debug!(
                    "Skipping immediate scan due to file-event debounce for path: {}",
                    path.display()
                );
                return false;
            }
        }

        *last_scan_at = Some(now);
        true
    }

    fn run_sqlite_query(
        db_path: &Path,
        sql: &str,
    ) -> Result<std::process::Output, Box<dyn std::error::Error + Send + Sync>> {
        Ok(std::process::Command::new("sqlite3")
            .arg("-readonly")
            .arg(db_path.to_str().unwrap())
            .arg(sql)
            .output()?)
    }

    fn schedule_catch_up_polling(&self) {
        let mut catch_up_until = self.state.catch_up_until.lock().unwrap();
        let should_log_start = catch_up_until.is_none();
        *catch_up_until = Some(Instant::now() + CATCH_UP_POLL_WINDOW);
        drop(catch_up_until);

        if should_log_start {
            info!(
                "Starting message catch-up polling window for {} seconds",
                CATCH_UP_POLL_WINDOW.as_secs()
            );
        }

        if self
            .state
            .catch_up_task_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let processor = self.clone();
            tokio::spawn(async move {
                processor.run_catch_up_polling().await;
            });
        }
    }

    async fn run_catch_up_polling(self) {
        let mut poll_tick = 0u32;
        loop {
            tokio::time::sleep(CATCH_UP_POLL_INTERVAL).await;

            let should_continue = {
                let mut catch_up_until = self.state.catch_up_until.lock().unwrap();
                match *catch_up_until {
                    Some(deadline) if Instant::now() < deadline => true,
                    _ => {
                        *catch_up_until = None;
                        false
                    }
                }
            };

            if !should_continue {
                self.state
                    .catch_up_task_running
                    .store(false, Ordering::SeqCst);

                let restart_needed = {
                    let catch_up_until = self.state.catch_up_until.lock().unwrap();
                    matches!(*catch_up_until, Some(deadline) if Instant::now() < deadline)
                };

                if restart_needed
                    && self
                        .state
                        .catch_up_task_running
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                {
                    continue;
                }

                info!("Message catch-up polling window ended");
                break;
            }

            poll_tick += 1;
            debug!(
                "Message catch-up polling tick {} started",
                poll_tick
            );

            if let Err(e) = self.scan_for_new_messages("catch-up polling") {
                error!("Failed to scan Messages database during catch-up polling: {}", e);
            }
        }
    }

    fn scan_for_new_messages(
        &self,
        trigger: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _scan_guard = self.state.scan_lock.lock().unwrap();
        let is_catch_up_polling = trigger == "catch-up polling";
        debug!("Scanning Messages database triggered by {}", trigger);

        let home_dir = env::var("HOME")?;
        let db_path = PathBuf::from(&home_dir).join("Library/Messages/chat.db");
        debug!("Using database: {:?}", db_path);

        let last_rowid;
        {
            let last_processed = LAST_PROCESSED_ROWID.lock().unwrap();
            last_rowid = *last_processed;
            debug!("Last processed ROWID: {}", last_rowid);
        }

        let sql = format!(
            "SELECT m.ROWID,
                    REPLACE(REPLACE(REPLACE(IFNULL(m.text, ''), char(13), ' '), char(10), ' '), '|', ' ')
             FROM message m
             WHERE m.ROWID > {}
             ORDER BY m.ROWID ASC
             LIMIT 50;",
            last_rowid
        );
        debug!("SQL query: {}", sql);

        debug!("Executing SQLite query...");
        let output = Self::run_sqlite_query(&db_path, &sql)?;

        if output.status.success() {
            let output_str = String::from_utf8_lossy(&output.stdout);
            debug!("SQLite output: {}", output_str);

            let messages = parse_sqlite_output(&output_str);
            debug!("Parsed {} messages", messages.len());
            let newest_scanned_rowid = get_last_rowid(&output_str).ok();
            let mut otp_found_in_this_scan = false;

            for (i, message) in messages.iter().enumerate() {
                debug!("Processing message {}: {}", i, message);
                if let Some(code) = parser::extract_verification_code(message) {
                    otp_found_in_this_scan = true;
                    info!(
                        "Found verification code in message via {}: {}",
                        trigger, code
                    );

                    let config = Config::load().unwrap_or_default();
                    if config.double_click_fill {
                        quick_fill::cache_code(&code);
                    }

                    if config.floating_window {
                        match ipc::spawn_floating_window(&code, "iMessage") {
                            Ok(child) => {
                                if config.double_click_fill {
                                    quick_fill::register_popup(&code, child);
                                }
                                debug!("Floating window spawned successfully");
                            }
                            Err(e) => error!("Failed to spawn floating window: {}", e),
                        }
                    } else if config.direct_input {
                        if let Err(e) = clipboard::auto_paste(true, &code) {
                            error!("Failed to direct input verification code: {}", e);
                        } else {
                            info!("Direct input verification code: {}", code);

                            if config.auto_enter {
                                if let Err(e) = clipboard::press_enter() {
                                    error!("Failed to press enter key: {}", e);
                                } else {
                                    info!("Auto-pressed enter key");
                                }
                            }
                        }
                    } else if let Err(e) = clipboard::copy_to_clipboard(&code) {
                        error!("Failed to copy verification code to clipboard: {}", e);
                    } else {
                        info!("Auto-copied verification code to clipboard: {}", code);

                        if config.auto_paste {
                            if let Err(e) = clipboard::auto_paste(false, &code) {
                                error!("Failed to auto-paste verification code: {}", e);
                            } else {
                                info!("Auto-pasted verification code: {}", code);

                                if config.auto_enter {
                                    if let Err(e) = clipboard::press_enter() {
                                        error!("Failed to press enter key: {}", e);
                                    } else {
                                        info!("Auto-pressed enter key");
                                    }
                                }
                            }
                        } else if config.auto_enter {
                            if let Err(e) = clipboard::press_enter() {
                                error!("Failed to press enter key: {}", e);
                            } else {
                                info!("Auto-pressed enter key");
                            }
                        }
                    }
                } else {
                    debug!("No verification code found in message");
                }
            }

            if otp_found_in_this_scan {
                info!(
                    "OTP found via {}, starting catch-up polling for {} seconds",
                    trigger,
                    CATCH_UP_POLL_WINDOW.as_secs()
                );
                self.schedule_catch_up_polling();
            } else if !is_catch_up_polling && !messages.is_empty() {
                info!(
                    "No OTP found in {} new Messages row(s); starting catch-up polling for {} seconds",
                    messages.len(),
                    CATCH_UP_POLL_WINDOW.as_secs()
                );
                self.schedule_catch_up_polling();
            }

            if !messages.is_empty() {
                if let Some(rowid) = newest_scanned_rowid {
                    info!(
                        "Messages scan via {} observed {} new row(s), ROWID {} -> {}",
                        trigger,
                        messages.len(),
                        last_rowid,
                        rowid
                    );
                    debug!("Updating last processed ROWID to {}", rowid);
                    let mut last_processed = LAST_PROCESSED_ROWID.lock().unwrap();
                    *last_processed = rowid;
                } else {
                    warn!("Failed to get last ROWID from output");
                }
            } else {
                let latest_rowid = Self::get_latest_message_rowid().unwrap_or(last_rowid);
                if is_catch_up_polling {
                    if latest_rowid == last_rowid {
                        debug!(
                            "Catch-up polling tick found no new Messages rows; latest ROWID is still {}",
                            latest_rowid
                        );
                    } else {
                        info!(
                            "Catch-up polling saw Messages ROWID advance from {} to {}, but this scan did not return pending rows",
                            last_rowid,
                            latest_rowid
                        );
                    }
                } else {
                    debug!(
                        "Messages scan via {} found no new rows; latest ROWID remains {}",
                        trigger,
                        latest_rowid
                    );
                }
            }
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("Error executing SQLite query: {}", stderr);
            debug!("Command status: {:?}", output.status);

            if stderr.contains("attempt to write a readonly database")
                || stderr.contains("permission denied")
                || stderr.contains("unable to open database")
            {
                warn!("Permission error detected when accessing Messages database");
                if !permissions::check_full_disk_access() {
                    permissions::show_permission_dialog();
                }
            }
        }

        Ok(())
    }
}

impl FileProcessor for MessageProcessor {
    fn get_watch_path(&self) -> PathBuf {
        let home_dir = env::var("HOME").expect("Failed to get HOME directory");
        PathBuf::from(&home_dir).join("Library/Messages")
    }

    fn get_file_pattern(&self) -> &str {
        ".db"
    }

    fn get_recursive_mode(&self) -> RecursiveMode {
        RecursiveMode::Recursive
    }

    fn process_file(
        &self,
        path: &Path,
        event_kind: &EventKind,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        debug!("process_file_event_kind: {:?}", event_kind);
        if !Self::is_relevant_messages_path(path) {
            debug!("Ignoring unrelated Messages path: {:?}", path);
            return Ok(());
        }

        if !Self::is_relevant_event(event_kind) {
            debug!("Ignoring unrelated Messages event: {:?}", event_kind);
            return Ok(());
        }

        debug!("Message file change detected: {:?}", path);
        debug!("检测到 Messages 相关数据库文件变化，可能有新消息");

        if !self.should_scan_immediately_for_file_event(path) {
            debug!(
                "Accepted Messages file event without immediate scan: kind={:?}, path={}",
                event_kind,
                path.display()
            );
            return Ok(());
        }

        info!(
            "Accepted Messages file event: kind={:?}, path={}",
            event_kind,
            path.display()
        );
        self.scan_for_new_messages("file event")
    }
}

fn parse_sqlite_output(output: &str) -> Vec<String> {
    let mut result = Vec::new();

    for line in output.lines() {
        if !line.trim().is_empty() {
            let parts: Vec<&str> = line.splitn(2, '|').collect();
            if parts.len() >= 2 {
                let id = parts[0].trim();
                let text = parts[1].trim();
                debug!("New message found with ID {}: {}", id, text);
                result.push(text.to_string());
            }
        }
    }

    result
}

fn get_last_rowid(output: &str) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    let lines: Vec<&str> = output.trim().lines().collect();
    if let Some(last_line) = lines.last().filter(|line| !line.is_empty()) {
        let parts: Vec<&str> = last_line.splitn(2, '|').collect();
        if !parts.is_empty() {
            return Ok(parts[0].parse()?);
        }
    }
    Err("No valid ROWID found".into())
}
