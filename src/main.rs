use git2::{DiffOptions, Repository, Status, StatusOptions};
use iced::advanced::graphics::core::Element;
use iced::keyboard::{self, key, Key, Modifiers};
use iced::widget::{
    button, column, container, image, row, scrollable, text, text_editor, text_input, Column, Row,
    Stack,
};
use iced::{color, Length, Size, Subscription, Task, Theme};
use iced_term::{SearchMatch, TerminalView};
use muda::{accelerator::Accelerator, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

use similar::{ChangeTag, TextDiff};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(feature = "stt")]
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

#[cfg(feature = "excalidraw")]
mod excalidraw;
mod log_server;
mod markdown;
mod plans_viewer;
mod services;
mod webview;

// New modules
mod agent;
mod config;
mod events;
mod tab;
mod theme;

use tab::{
    AgentActivityState, AgentBackendConfig, AgentSession, FileViewerOverlay, TabKind, TerminalTab,
};

// Start with just config for now to avoid conflicts
use config::{
    AgentPreset, BottomTerminalConfig, Config, QuickCommand, WorkspaceColor, WorkspaceConfig,
    WorkspaceTabConfig, WorkspacesFile,
};
use events::SidebarMode;
use theme::AppTheme;

// Freeze debugging
static FREEZE_DEBUG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// Main thread heartbeat for freeze detection
static MAIN_THREAD_HEARTBEAT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static MAIN_THREAD_LAST_EVENT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

// Store main thread ID so watchdog can signal it for backtrace
static MAIN_THREAD_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// ---- Agent webview IPC bridge (Step 4 of TRU-29) -------------------------
//
// The agent chat webview posts JSON messages back to Rust via wry's
// `with_ipc_handler`. The handler is set once at webview construction (wry
// has no API to swap it later), so it's stateless: it pushes parsed messages
// onto a global mpsc, and an Iced subscription drains the mpsc into typed
// `Event::AgentWebviewIpc` variants for the update loop to handle.
//
// Multi-tab routing: the JS side stamps every postMessage with the active
// tab's id (set by Rust via evaluate_script on tab activation). The
// dispatcher in update() reads tabId from the message and routes the prompt
// or stop signal to the right tab.
#[derive(Debug, Clone)]
pub enum AgentIpcMessage {
    Submit { tab_id: usize, text: String },
    Stop { tab_id: usize },
}

static AGENT_IPC_TX: OnceLock<tokio::sync::mpsc::UnboundedSender<AgentIpcMessage>> =
    OnceLock::new();
static AGENT_IPC_RX: OnceLock<
    Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<AgentIpcMessage>>>,
> = OnceLock::new();

fn init_agent_ipc_channel() {
    AGENT_IPC_TX.get_or_init(|| {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentIpcMessage>();
        let _ = AGENT_IPC_RX.set(Mutex::new(Some(rx)));
        tx
    });
}

/// Iced subscription that drains the global IPC mpsc into typed events.
/// Built once on first poll (function pointer, no captures); the receiver is
/// taken out of `AGENT_IPC_RX` at that point and lives inside the stream
/// for the rest of the process. Subsequent rebuilds (which happen if the
/// subscription's identity hash changes) would receive an empty stream —
/// but the identity here is stable (no `with(...)` value), so we get exactly
/// one runner.
fn agent_ipc_stream() -> impl iced::futures::Stream<Item = Event> + Send + 'static {
    use iced::futures::StreamExt;
    let rx = AGENT_IPC_RX
        .get()
        .and_then(|m| m.lock().ok().and_then(|mut g| g.take()));
    let stream: tokio_stream::wrappers::UnboundedReceiverStream<AgentIpcMessage> = match rx {
        Some(rx) => tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
        None => {
            // Channel already drained or not initialized — produce an empty
            // stream by closing a fresh channel immediately.
            let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentIpcMessage>();
            tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
        }
    };
    stream.map(Event::AgentWebviewIpc)
}

/// Embedded HTML scaffold for the agent chat surface (Step 4 of TRU-29).
const AGENT_CHAT_HTML: &str = include_str!("../assets/agent_chat.html");

/// Push one `AgentEvent` into the live webview via `window.__appendEvent(...)`.
/// Caller is responsible for ensuring the agent webview is the active one;
/// this is a no-op if no webview exists.
fn push_agent_event_to_webview(ev: &tab::AgentEvent) {
    let payload = match ev {
        tab::AgentEvent::Other(value) => value.clone(),
        // Other typed variants don't currently fire (Step 3 emits only
        // `Other`). When the typed parser lands these will be serialized
        // into a richer JS-side shape.
        _ => return,
    };
    let payload_json = serde_json::to_string(&payload).unwrap_or_else(|_| "null".to_string());
    webview::evaluate_script(&format!("window.__appendEvent({})", payload_json));
}

/// Set the active tab id on the JS side. The webview tags every IPC message
/// with this id so the Rust dispatcher can route the prompt/stop to the
/// correct tab (multiple agent tabs may share the singleton webview, swapping
/// in and out via tab activation).
fn set_agent_webview_tab_id(tab_id: usize) {
    webview::evaluate_script(&format!("window.__setTabId({})", tab_id));
}

/// Reset the chat surface: clears all rendered messages and sets status to Idle.
fn reset_agent_webview() {
    webview::evaluate_script("window.__resetConversation()");
}

/// Build the IPC handler closure to install at agent-webview creation. Parses
/// each `window.ipc.postMessage(...)` body and forwards as a typed message
/// into the global IPC channel for the Iced subscription to pick up.
fn agent_ipc_handler() -> webview::IpcHandler {
    Box::new(|body: String| {
        let Some(tx) = AGENT_IPC_TX.get() else {
            eprintln!("[agent-ipc] tx not initialized; dropping {}", body);
            return;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) else {
            eprintln!("[agent-ipc] non-JSON body: {}", body);
            return;
        };
        let tab_id = value
            .get("tabId")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);
        let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let Some(tab_id) = tab_id else {
            eprintln!("[agent-ipc] missing tabId in {}", body);
            return;
        };
        let msg = match kind {
            "submit" => {
                let text = value
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                AgentIpcMessage::Submit { tab_id, text }
            }
            "stop" => AgentIpcMessage::Stop { tab_id },
            _ => {
                eprintln!("[agent-ipc] unknown type {:?}", kind);
                return;
            }
        };
        let _ = tx.send(msg);
    })
}

fn start_freeze_watchdog() {
    if !freeze_debug_enabled() {
        return;
    }

    // Store main thread's pthread ID for signaling
    #[cfg(unix)]
    {
        let tid = unsafe { libc::pthread_self() } as u64;
        MAIN_THREAD_ID.store(tid, std::sync::atomic::Ordering::Relaxed);

        // Install SIGUSR1 handler that prints backtrace
        unsafe {
            libc::signal(
                libc::SIGUSR1,
                freeze_backtrace_handler as *const () as libc::sighandler_t,
            );
        }
    }

    std::thread::Builder::new()
        .name("freeze-watchdog".to_string())
        .spawn(|| {
            let mut last_beat = 0u64;
            let mut stall_start: Option<std::time::Instant> = None;
            let mut backtrace_sent = false;
            let mut log_count = 0u32;
            loop {
                std::thread::sleep(Duration::from_millis(500));
                let current = MAIN_THREAD_HEARTBEAT.load(std::sync::atomic::Ordering::Relaxed);
                if current == last_beat {
                    // Main thread hasn't ticked
                    if stall_start.is_none() {
                        stall_start = Some(std::time::Instant::now());
                        backtrace_sent = false;
                        log_count = 0;
                    }
                    let elapsed = stall_start.unwrap().elapsed();

                    // Log the first few stall messages, then go quiet
                    if elapsed.as_secs() >= 2 && log_count < 10 {
                        log_count += 1;
                        let event_name = MAIN_THREAD_LAST_EVENT
                            .lock()
                            .ok()
                            .and_then(|g| g.clone())
                            .unwrap_or_else(|| "unknown".to_string());
                        eprintln!(
                            "[FREEZE-WATCHDOG] Main thread stalled for {:.1}s! Last event: {}",
                            elapsed.as_secs_f64(),
                            event_name
                        );
                    }

                    // Send SIGUSR1 at 5s to capture backtrace (once)
                    #[cfg(unix)]
                    if elapsed.as_secs() >= 5 && !backtrace_sent {
                        backtrace_sent = true;
                        let tid = MAIN_THREAD_ID.load(std::sync::atomic::Ordering::Relaxed);
                        if tid != 0 {
                            eprintln!(
                                "\n[FREEZE-WATCHDOG] ========================================"
                            );
                            eprintln!("[FREEZE-WATCHDOG] Capturing backtrace via SIGUSR1...");
                            eprintln!(
                                "[FREEZE-WATCHDOG] ========================================\n"
                            );
                            unsafe {
                                libc::pthread_kill(tid as libc::pthread_t, libc::SIGUSR1);
                            }
                            // Give the signal handler time to print before any more logs
                            std::thread::sleep(Duration::from_secs(2));
                        }
                    }

                    // After 10 logs, only log every 60s
                    if log_count >= 10 {
                        let secs = elapsed.as_secs();
                        if secs.is_multiple_of(60) {
                            eprintln!(
                                "[FREEZE-WATCHDOG] Still stalled after {}m. Force quit to restart.",
                                secs / 60
                            );
                        }
                        std::thread::sleep(Duration::from_millis(1500));
                    }
                } else {
                    if let Some(start) = stall_start.take() {
                        let dur = start.elapsed();
                        if dur.as_secs() >= 2 {
                            eprintln!(
                                "[FREEZE-WATCHDOG] Main thread recovered after {:.1}s stall",
                                dur.as_secs_f64()
                            );
                        }
                    }
                    last_beat = current;
                }
            }
        })
        .ok();
}

/// Signal handler: prints backtrace when SIGUSR1 is received on the main thread.
/// This runs IN the frozen main thread's context, so the backtrace shows exactly
/// where it's stuck.
///
/// IMPORTANT: This handler must be async-signal-safe. No heap allocation allowed!
/// `std::backtrace::Backtrace` and `format!()` allocate, so if the signal arrives
/// while the thread is inside malloc, it would deadlock/crash with
/// "BUG IN LIBMALLOC: ulock_wait failure".
///
/// Instead we use `libc::backtrace()` to capture raw frame pointers into a
/// stack-allocated buffer, then write hex addresses to stderr with `libc::write()`.
/// The addresses can be symbolicated later with:
///   atos -o target/release/gitterm -l 0x<load_addr> <addr1> <addr2> ...
#[cfg(unix)]
extern "C" fn freeze_backtrace_handler(_sig: libc::c_int) {
    unsafe {
        // All buffers on the stack — zero heap allocation
        let mut frames: [*mut libc::c_void; 128] = [std::ptr::null_mut(); 128];
        let n = libc::backtrace(frames.as_mut_ptr(), 128);

        let header = b"\n[FREEZE-WATCHDOG] === MAIN THREAD BACKTRACE (raw frames) ===\n";
        libc::write(2, header.as_ptr() as *const libc::c_void, header.len());

        // Write each frame address as hex, one per line
        // Format: "  [NN] 0xHHHHHHHHHHHHHHHH\n"
        let hex_chars = b"0123456789abcdef";
        for i in 0..n as usize {
            let addr = frames[i] as usize;
            // Build line in stack buffer: "  [NN] 0x________________\n"
            let mut line: [u8; 32] = [b' '; 32];
            // Frame index
            line[2] = b'[';
            if i >= 100 {
                line[3] = hex_chars[(i / 100) % 10];
                line[4] = hex_chars[(i / 10) % 10];
                line[5] = hex_chars[i % 10];
                line[6] = b']';
                line[7] = b' ';
                line[8] = b'0';
                line[9] = b'x';
            } else if i >= 10 {
                line[3] = hex_chars[(i / 10) % 10];
                line[4] = hex_chars[i % 10];
                line[5] = b']';
                line[6] = b' ';
                line[7] = b'0';
                line[8] = b'x';
            } else {
                line[3] = hex_chars[i % 10];
                line[4] = b']';
                line[5] = b' ';
                line[6] = b'0';
                line[7] = b'x';
            }
            // Write 16 hex digits for the address (always 64-bit)
            let hex_start = if i >= 100 {
                10
            } else if i >= 10 {
                9
            } else {
                8
            };
            for digit in 0..16 {
                let nibble = (addr >> (60 - digit * 4)) & 0xf;
                line[hex_start + digit] = hex_chars[nibble];
            }
            let line_len = hex_start + 16;
            line[line_len] = b'\n';
            libc::write(2, line.as_ptr() as *const libc::c_void, line_len + 1);
        }

        let footer = b"[FREEZE-WATCHDOG] === END BACKTRACE ===\n\n";
        libc::write(2, footer.as_ptr() as *const libc::c_void, footer.len());
    }
}

/// Record that the main thread processed an event
fn heartbeat(event_name: &str) {
    MAIN_THREAD_HEARTBEAT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if freeze_debug_enabled() {
        if let Ok(mut guard) = MAIN_THREAD_LAST_EVENT.try_lock() {
            *guard = Some(event_name.to_string());
        }
    }
}

/// Truncate a string to at most `max_bytes` bytes at a valid UTF-8 char boundary.
fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Percent-encode RFC 3986 reserved characters in a query-string value.
/// Used for `?plan=<name>` so plan filenames with spaces or punctuation
/// round-trip cleanly into the viewer's URL.
fn urlencoding_simple(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn freeze_debug_enabled() -> bool {
    FREEZE_DEBUG.load(std::sync::atomic::Ordering::Relaxed)
        || std::env::var("GITTERM_DEBUG_FREEZES").is_ok()
}

macro_rules! freeze_debug {
    ($($arg:tt)*) => {{
        if freeze_debug_enabled() {
            eprintln!("[FREEZE-DEBUG] {}", format_args!($($arg)*));
        }
    }};
}

macro_rules! freeze_time {
    ($label:expr, $block:block) => {{
        let _start = std::time::Instant::now();
        let result = $block;
        let _elapsed = _start.elapsed();
        if _elapsed > Duration::from_millis(100) {
            freeze_debug!("{} took {}ms", $label, _elapsed.as_millis());
        }
        result
    }};
}

// Menu item IDs stored globally for event matching
static MENU_IDS: OnceLock<MenuIds> = OnceLock::new();

#[derive(Debug)]
struct MenuIds {
    increase_terminal_font: muda::MenuId,
    decrease_terminal_font: muda::MenuId,
    increase_ui_font: muda::MenuId,
    decrease_ui_font: muda::MenuId,
    toggle_theme: muda::MenuId,
    toggle_log_server: muda::MenuId,
    clear_terminal: muda::MenuId,
    open_plans_viewer: muda::MenuId,
}

fn setup_menu_bar() {
    // Create native macOS menu bar
    let menu = Menu::new();

    // App menu (GitTerm)
    let app_menu = Submenu::new("GitTerm", true);
    app_menu
        .append_items(&[
            &PredefinedMenuItem::about(None, None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::services(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::quit(None),
        ])
        .unwrap();

    // View menu (font size, theme)
    let view_menu = Submenu::new("View", true);

    // Terminal font submenu
    let terminal_font_menu = Submenu::new("Terminal Font", true);
    let increase_terminal_font = MenuItem::new(
        "Increase",
        true,
        Some(Accelerator::new(
            Some(muda::accelerator::Modifiers::META),
            muda::accelerator::Code::Equal,
        )),
    );
    let decrease_terminal_font = MenuItem::new(
        "Decrease",
        true,
        Some(Accelerator::new(
            Some(muda::accelerator::Modifiers::META),
            muda::accelerator::Code::Minus,
        )),
    );
    let clear_terminal = MenuItem::new(
        "Clear Terminal",
        true,
        Some(Accelerator::new(
            Some(muda::accelerator::Modifiers::META),
            muda::accelerator::Code::KeyK,
        )),
    );
    terminal_font_menu
        .append_items(&[
            &increase_terminal_font,
            &decrease_terminal_font,
            &clear_terminal,
        ])
        .unwrap();

    // UI font submenu
    let ui_font_menu = Submenu::new("Sidebar Font", true);
    let increase_ui_font = MenuItem::new(
        "Increase",
        true,
        Some(Accelerator::new(
            Some(muda::accelerator::Modifiers::META | muda::accelerator::Modifiers::SHIFT),
            muda::accelerator::Code::Equal,
        )),
    );
    let decrease_ui_font = MenuItem::new(
        "Decrease",
        true,
        Some(Accelerator::new(
            Some(muda::accelerator::Modifiers::META | muda::accelerator::Modifiers::SHIFT),
            muda::accelerator::Code::Minus,
        )),
    );
    ui_font_menu
        .append_items(&[&increase_ui_font, &decrease_ui_font])
        .unwrap();

    let toggle_theme = MenuItem::new(
        "Toggle Light/Dark Theme",
        true,
        Some(Accelerator::new(
            Some(muda::accelerator::Modifiers::META | muda::accelerator::Modifiers::SHIFT),
            muda::accelerator::Code::KeyT,
        )),
    );
    let toggle_log_server = MenuItem::new(
        "Toggle Local Log Server",
        true,
        Some(Accelerator::new(
            Some(muda::accelerator::Modifiers::META | muda::accelerator::Modifiers::SHIFT),
            muda::accelerator::Code::KeyL,
        )),
    );
    let open_plans_viewer = MenuItem::new(
        "Plans Viewer",
        true,
        Some(Accelerator::new(
            Some(muda::accelerator::Modifiers::META | muda::accelerator::Modifiers::SHIFT),
            muda::accelerator::Code::KeyP,
        )),
    );

    view_menu
        .append_items(&[
            &terminal_font_menu,
            &ui_font_menu,
            &PredefinedMenuItem::separator(),
            &toggle_theme,
            &toggle_log_server,
            &PredefinedMenuItem::separator(),
            &open_plans_viewer,
        ])
        .unwrap();

    // Window menu
    let window_menu = Submenu::new("Window", true);
    window_menu
        .append_items(&[
            &PredefinedMenuItem::minimize(None),
            &PredefinedMenuItem::maximize(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::fullscreen(None),
        ])
        .unwrap();

    menu.append_items(&[&app_menu, &view_menu, &window_menu])
        .unwrap();

    // Store menu IDs for event handling
    let _ = MENU_IDS.set(MenuIds {
        increase_terminal_font: increase_terminal_font.id().clone(),
        decrease_terminal_font: decrease_terminal_font.id().clone(),
        increase_ui_font: increase_ui_font.id().clone(),
        decrease_ui_font: decrease_ui_font.id().clone(),
        toggle_theme: toggle_theme.id().clone(),
        toggle_log_server: toggle_log_server.id().clone(),
        clear_terminal: clear_terminal.id().clone(),
        open_plans_viewer: open_plans_viewer.id().clone(),
    });

    // Initialize menu for macOS - this must happen after NSApp exists
    #[cfg(target_os = "macos")]
    menu.init_for_nsapp();

    // Leak the menu to keep it alive for the lifetime of the app
    Box::leak(Box::new(menu));
}

// === Speech-to-Text helpers ===

#[cfg(feature = "stt")]
const STT_MIN_RECORDING_SECONDS: f32 = 0.25;
#[cfg(feature = "stt")]
const STT_MIN_RMS: f32 = 0.001;
#[cfg(feature = "stt")]
const STT_MIN_PEAK: f32 = 0.01;

#[cfg(feature = "stt")]
fn stt_model_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("gitterm")
        .join("models")
        .join("ggml-base.en.bin")
}

#[cfg(feature = "stt")]
fn stt_audio_stats(samples: &[f32]) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }

    let mut sum_squares = 0.0f64;
    let mut peak = 0.0f32;
    for &sample in samples {
        let abs = sample.abs();
        peak = peak.max(abs);
        let sample = sample as f64;
        sum_squares += sample * sample;
    }

    let rms = (sum_squares / samples.len() as f64).sqrt() as f32;
    (rms, peak)
}

#[cfg(feature = "stt")]
fn stt_start_recording(audio_buffer: Arc<Mutex<Vec<f32>>>) -> Result<(cpal::Stream, u32), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| "No input device available".to_string())?;

    let config = device
        .default_input_config()
        .map_err(|e| format!("Failed to get input config: {}", e))?;

    let sample_rate = config.sample_rate().0;
    let device_name = device
        .name()
        .unwrap_or_else(|_| "<unknown input device>".to_string());
    eprintln!(
        "[STT] Starting recording: device={} sample_rate={} channels={} format={:?}",
        device_name,
        sample_rate,
        config.channels(),
        config.sample_format()
    );

    // Clear existing buffer
    {
        let mut buf = audio_buffer.lock().unwrap();
        buf.clear();
    }

    let channels = config.channels() as usize;
    let buf = audio_buffer.clone();

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device
            .build_input_stream(
                &config.into(),
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let mut buf = buf.lock().unwrap();
                    // Convert to mono by averaging channels
                    if channels == 1 {
                        buf.extend_from_slice(data);
                    } else {
                        for chunk in data.chunks(channels) {
                            let sum: f32 = chunk.iter().sum();
                            buf.push(sum / channels as f32);
                        }
                    }
                },
                |err| eprintln!("[STT] Audio stream error: {}", err),
                None,
            )
            .map_err(|e| format!("Failed to build input stream: {}", e))?,
        cpal::SampleFormat::I16 => device
            .build_input_stream(
                &config.into(),
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    let mut buf = buf.lock().unwrap();
                    if channels == 1 {
                        buf.extend(data.iter().map(|&s| s as f32 / 32768.0));
                    } else {
                        for chunk in data.chunks(channels) {
                            let sum: f32 = chunk.iter().map(|&s| s as f32 / 32768.0).sum();
                            buf.push(sum / channels as f32);
                        }
                    }
                },
                |err| eprintln!("[STT] Audio stream error: {}", err),
                None,
            )
            .map_err(|e| format!("Failed to build input stream: {}", e))?,
        format => return Err(format!("Unsupported sample format: {:?}", format)),
    };

    stream
        .play()
        .map_err(|e| format!("Failed to start stream: {}", e))?;

    Ok((stream, sample_rate))
}

#[cfg(feature = "stt")]
fn stt_transcribe(
    ctx: Arc<whisper_rs::WhisperContext>,
    mono_samples: Vec<f32>,
    input_sample_rate: u32,
) -> Result<String, String> {
    let input_rate = input_sample_rate as usize;
    let output_rate = 16000usize;

    // Resample to 16kHz for Whisper using linear interpolation
    let resampled = if input_rate != output_rate {
        let ratio = input_rate as f64 / output_rate as f64;
        let output_len = (mono_samples.len() as f64 / ratio) as usize;
        let mut output = Vec::with_capacity(output_len);
        for i in 0..output_len {
            let src_idx = i as f64 * ratio;
            let idx0 = src_idx as usize;
            let frac = src_idx - idx0 as f64;
            let s0 = mono_samples.get(idx0).copied().unwrap_or(0.0);
            let s1 = mono_samples.get(idx0 + 1).copied().unwrap_or(s0);
            output.push(s0 + (s1 - s0) * frac as f32);
        }
        output
    } else {
        mono_samples
    };

    // Run Whisper
    let mut state = ctx
        .create_state()
        .map_err(|e| format!("Failed to create whisper state: {}", e))?;

    let mut params =
        whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some("en"));
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_suppress_blank(true);
    params.set_no_speech_thold(0.6);

    state
        .full(params, &resampled)
        .map_err(|e| format!("Whisper transcription failed: {}", e))?;

    let num_segments = state.full_n_segments();
    let mut result = String::new();
    for i in 0..num_segments {
        if let Some(segment) = state.get_segment(i) {
            if let Ok(segment_text) = segment.to_str_lossy() {
                result.push_str(&segment_text);
            }
        }
    }

    Ok(result.trim().to_string())
}

fn main() -> iced::Result {
    // Print instance information for multi-instance support
    config::print_instance_info();

    // Start freeze detection watchdog
    start_freeze_watchdog();

    // Initialize the global agent-webview IPC channel before any webview can
    // post messages into it. The receiver is parked for the subscription to
    // take on first build.
    init_agent_ipc_channel();

    // Load app icon from embedded PNG
    let icon = iced::window::icon::from_file_data(include_bytes!("../assets/icon.png"), None).ok();

    iced::application(App::new, App::update, App::view)
        .title(App::title)
        .window_size(Size {
            width: 1400.0,
            height: 800.0,
        })
        .window(iced::window::Settings {
            icon,
            ..Default::default()
        })
        .subscription(App::subscription)
        .run()
}

// Git file entry
#[derive(Debug, Clone)]
struct FileEntry {
    path: String,
    status: String,
    is_staged: bool,
}

// File tree entry for explorer
#[derive(Debug, Clone)]
struct FileTreeEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

#[derive(Debug, Clone)]
struct SyntaxHighlightSegment {
    text: String,
    color: iced::Color,
}

#[derive(Debug, Clone)]
pub(crate) struct SyntaxHighlightLine {
    segments: Vec<SyntaxHighlightSegment>,
}

// Claude config scope
#[derive(Debug, Clone, PartialEq)]
enum ConfigScope {
    User,
    Project,
}

// Claude config item (skill, plugin, server, hook, or setting)
#[derive(Debug, Clone)]
struct ClaudeConfigItem {
    name: String,
    file_path: PathBuf,
    scope: ConfigScope,
}

// Claude sidebar config tree
#[derive(Debug, Clone, Default)]
struct ClaudeConfig {
    skills: Vec<ClaudeConfigItem>,
    plugins: Vec<ClaudeConfigItem>,
    mcp_servers: Vec<ClaudeConfigItem>,
    hooks: Vec<ClaudeConfigItem>,
    settings: Vec<ClaudeConfigItem>,
    expanded: HashSet<String>,
    selected_item: Option<(String, usize)>,
}

// Inline change for word-level diffs
#[derive(Debug, Clone)]
struct InlineChange {
    change_type: ChangeType,
    value: String,
}

#[derive(Debug, Clone, PartialEq)]
enum ChangeType {
    Equal,
    Insert,
    Delete,
}

// Diff line for display
#[derive(Debug, Clone)]
struct DiffLine {
    content: String,
    line_type: DiffLineType,
    old_line_num: Option<u32>,
    new_line_num: Option<u32>,
    inline_changes: Option<Vec<InlineChange>>,
}

#[derive(Debug, Clone, PartialEq)]
enum DiffLineType {
    Context,
    Addition,
    Deletion,
    Header,
}

// Search state for terminal scrollback
#[derive(Debug, Clone, Default)]
struct SearchState {
    is_active: bool,
    query: String,
    matches: Vec<SearchMatch>,
    current_match: usize,
}

// Console panel constants
const CONSOLE_HEADER_HEIGHT: f32 = 32.0;
const CONSOLE_DIVIDER_HEIGHT: f32 = 3.0;

const MAX_CONSOLE_LINES: usize = 1000;
const MAX_INLINE_WEBVIEW_BYTES: u64 = 1_500_000;
const MAX_FULL_TEXT_LOAD_BYTES: u64 = 1_000_000;
const LARGE_TEXT_PREVIEW_BYTES: usize = 256 * 1024;
const LARGE_TEXT_PREVIEW_LINES: usize = 2000;
const MAX_SYNTAX_HIGHLIGHT_BYTES: usize = 96 * 1024;
const MAX_SYNTAX_HIGHLIGHT_LINES: usize = 1200;
const MAX_SYNTAX_HIGHLIGHT_SEGMENTS: usize = 8000;
const MAX_FILE_VIEW_RENDER_LINES: usize = 1200;
const MAX_FILE_VIEW_RENDER_LINES_WITH_SYNTAX: usize = 1200;
const MAX_DIFF_SYNTAX_HIGHLIGHT_BYTES: usize = 768 * 1024;
const MAX_DIFF_SYNTAX_HIGHLIGHT_LINES: usize = 900;
const MAX_DIFF_SYNTAX_SEGMENTS: usize = 9000;
const MAX_DIFF_VIEW_RENDER_LINES: usize = 1200;
const SYNTAX_HIGHLIGHT_CACHE_MAX_ENTRIES: usize = 64;
const DIFF_SYNTAX_CACHE_MAX_ENTRIES: usize = 64;
const FILE_SYNTAX_INITIAL_LINES: usize = 120;
const FILE_SYNTAX_SCROLL_PREFETCH_LINES: usize = 220;
const FILE_VIEW_LINE_HEIGHT_ESTIMATE: f32 = 22.0;
const LOADING_INDICATOR_DELAY_MS: u64 = 120;
const PERF_REPORT_INTERVAL_MS: u64 = 15000;

fn perf_enabled() -> bool {
    static PERF_ENABLED: OnceLock<bool> = OnceLock::new();
    *PERF_ENABLED.get_or_init(|| {
        std::env::var("GITTERM_PERF")
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    })
}

macro_rules! perf_log {
    ($($arg:tt)*) => {{
        if perf_enabled() {
            eprintln!("[perf] {}", format_args!($($arg)*));
        }
    }};
}

fn maybe_log_file_view_build(
    path: Option<&Path>,
    total_lines: usize,
    rendered_lines: usize,
    syntax: bool,
    took: Duration,
) {
    if !perf_enabled() {
        return;
    }
    let took_ms = took.as_millis();
    if took_ms < 16 {
        return;
    }

    static LAST_LOG: OnceLock<Mutex<(u64, Instant)>> = OnceLock::new();
    let path_hash = path
        .map(|p| {
            let mut hasher = DefaultHasher::new();
            p.hash(&mut hasher);
            hasher.finish()
        })
        .unwrap_or(0);
    let gate =
        LAST_LOG.get_or_init(|| Mutex::new((u64::MAX, Instant::now() - Duration::from_secs(60))));

    let mut should_log = false;
    if let Ok(mut last) = gate.lock() {
        if last.0 != path_hash || last.1.elapsed() >= Duration::from_secs(2) {
            *last = (path_hash, Instant::now());
            should_log = true;
        }
    } else {
        should_log = true;
    }

    if should_log {
        let path_display = path
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<none>".to_string());
        perf_log!(
            "file_view_build path={} total_lines={} rendered_lines={} syntax={} took={}ms",
            path_display,
            total_lines,
            rendered_lines,
            syntax,
            took_ms
        );
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct SyntaxHighlightCacheKey {
    path: PathBuf,
    modified_unix_nanos: u128,
    file_len: u64,
    is_dark_theme: bool,
    line_count: usize,
}

#[derive(Debug, Clone)]
struct SyntaxHighlightCacheEntry {
    lines: Option<Vec<SyntaxHighlightLine>>,
    notice: Option<String>,
}

#[derive(Default)]
struct SyntaxHighlightCache {
    entries: HashMap<SyntaxHighlightCacheKey, SyntaxHighlightCacheEntry>,
    lru: VecDeque<SyntaxHighlightCacheKey>,
}

impl SyntaxHighlightCache {
    fn get(&mut self, key: &SyntaxHighlightCacheKey) -> Option<SyntaxHighlightCacheEntry> {
        let entry = self.entries.get(key).cloned()?;
        if let Some(pos) = self.lru.iter().position(|existing| existing == key) {
            self.lru.remove(pos);
        }
        self.lru.push_back(key.clone());
        Some(entry)
    }

    fn put(&mut self, key: SyntaxHighlightCacheKey, entry: SyntaxHighlightCacheEntry) {
        if self.entries.contains_key(&key) {
            if let Some(pos) = self.lru.iter().position(|existing| existing == &key) {
                self.lru.remove(pos);
            }
        }

        self.entries.insert(key.clone(), entry);
        self.lru.push_back(key);

        while self.entries.len() > SYNTAX_HIGHLIGHT_CACHE_MAX_ENTRIES {
            if let Some(evicted) = self.lru.pop_front() {
                self.entries.remove(&evicted);
            } else {
                break;
            }
        }
    }
}

fn syntax_highlight_cache() -> &'static Mutex<SyntaxHighlightCache> {
    static CACHE: OnceLock<Mutex<SyntaxHighlightCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(SyntaxHighlightCache::default()))
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct FileVersionSignature {
    modified_unix_nanos: u128,
    file_len: u64,
}

fn file_version_signature(path: &Path) -> Option<FileVersionSignature> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified_unix_nanos = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();

    Some(FileVersionSignature {
        modified_unix_nanos,
        file_len: metadata.len(),
    })
}

fn syntax_highlight_cache_key(
    path: &Path,
    is_dark_theme: bool,
    line_count: usize,
) -> Option<SyntaxHighlightCacheKey> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified_unix_nanos = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();

    Some(SyntaxHighlightCacheKey {
        path: path.to_path_buf(),
        modified_unix_nanos,
        file_len: metadata.len(),
        is_dark_theme,
        line_count,
    })
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct DiffSyntaxCacheKey {
    file_path: String,
    is_staged: bool,
    is_dark_theme: bool,
    line_count: usize,
    content_hash: u64,
}

#[derive(Debug, Clone)]
struct DiffSyntaxCacheEntry {
    lines: Option<Vec<Vec<SyntaxHighlightSegment>>>,
    notice: Option<String>,
}

#[derive(Default)]
struct DiffSyntaxCache {
    entries: HashMap<DiffSyntaxCacheKey, DiffSyntaxCacheEntry>,
    lru: VecDeque<DiffSyntaxCacheKey>,
}

impl DiffSyntaxCache {
    fn get(&mut self, key: &DiffSyntaxCacheKey) -> Option<DiffSyntaxCacheEntry> {
        let entry = self.entries.get(key).cloned()?;
        if let Some(pos) = self.lru.iter().position(|existing| existing == key) {
            self.lru.remove(pos);
        }
        self.lru.push_back(key.clone());
        Some(entry)
    }

    fn put(&mut self, key: DiffSyntaxCacheKey, entry: DiffSyntaxCacheEntry) {
        if self.entries.contains_key(&key) {
            if let Some(pos) = self.lru.iter().position(|existing| existing == &key) {
                self.lru.remove(pos);
            }
        }

        self.entries.insert(key.clone(), entry);
        self.lru.push_back(key);

        while self.entries.len() > DIFF_SYNTAX_CACHE_MAX_ENTRIES {
            if let Some(evicted) = self.lru.pop_front() {
                self.entries.remove(&evicted);
            } else {
                break;
            }
        }
    }
}

fn diff_syntax_cache() -> &'static Mutex<DiffSyntaxCache> {
    static CACHE: OnceLock<Mutex<DiffSyntaxCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(DiffSyntaxCache::default()))
}

fn diff_line_type_code(line_type: &DiffLineType) -> u8 {
    match line_type {
        DiffLineType::Context => 0,
        DiffLineType::Addition => 1,
        DiffLineType::Deletion => 2,
        DiffLineType::Header => 3,
    }
}

fn hash_diff_lines(diff_lines: &[DiffLine]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for line in diff_lines {
        diff_line_type_code(&line.line_type).hash(&mut hasher);
        line.content.hash(&mut hasher);
        line.old_line_num.hash(&mut hasher);
        line.new_line_num.hash(&mut hasher);
    }
    hasher.finish()
}

fn syntect_syntax_set() -> &'static SyntaxSet {
    static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn syntect_theme_set() -> &'static ThemeSet {
    static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();
    THEME_SET.get_or_init(ThemeSet::load_defaults)
}

fn syntect_theme_for(is_dark_theme: bool) -> &'static syntect::highlighting::Theme {
    let theme_set = syntect_theme_set();
    let preferred = if is_dark_theme {
        [
            "base16-eighties.dark",
            "base16-ocean.dark",
            "Solarized (dark)",
        ]
    } else {
        ["InspiredGitHub", "base16-ocean.light", "Solarized (light)"]
    };

    for name in preferred {
        if let Some(theme) = theme_set.themes.get(name) {
            return theme;
        }
    }

    theme_set
        .themes
        .values()
        .next()
        .expect("syntect default themes should not be empty")
}

fn warm_syntect_engine() {
    let started = Instant::now();
    let syntax_set = syntect_syntax_set();
    let _ = syntect_theme_set();

    for sample in [
        "warmup.rs",
        "warmup.ts",
        "warmup.tsx",
        "warmup.js",
        "warmup.jsx",
        "warmup.json",
        "warmup.md",
        "warmup.toml",
        "warmup.yaml",
        "warmup.html",
        "warmup.css",
        "warmup.sh",
    ] {
        let _ = syntect_syntax_for_path(Path::new(sample));
    }

    for token in [
        "rust",
        "typescript",
        "tsx",
        "javascript",
        "json",
        "markdown",
    ] {
        let _ = syntax_set.find_syntax_by_token(token);
    }

    let _ = syntect_theme_for(true);
    let _ = syntect_theme_for(false);

    perf_log!("syntect warmup took={}ms", started.elapsed().as_millis());
}

fn syntect_syntax_for_path(path: &Path) -> &'static SyntaxReference {
    let syntax_set = syntect_syntax_set();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext = ext.to_ascii_lowercase();
        if let Some(syntax) = syntax_set.find_syntax_by_extension(&ext) {
            return syntax;
        }

        // Some packaged syntax sets do not register TS/TSX extensions.
        // Fall back to close JavaScript/TypeScript aliases before plain text.
        let alias_extensions: &[&str] = match ext.as_str() {
            "ts" => &["tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"],
            "tsx" => &["ts", "jsx", "js", "mjs", "cjs"],
            "mts" | "cts" => &["ts", "js", "mjs", "cjs"],
            "jsx" => &["js", "tsx", "ts", "mjs", "cjs"],
            "mjs" | "cjs" => &["js", "jsx", "ts", "tsx"],
            _ => &[],
        };
        for alias in alias_extensions {
            if let Some(syntax) = syntax_set.find_syntax_by_extension(alias) {
                return syntax;
            }
        }

        let alias_tokens: &[&str] = match ext.as_str() {
            "ts" | "tsx" | "mts" | "cts" => &["typescript", "ts", "tsx", "javascript", "js", "jsx"],
            "js" | "jsx" | "mjs" | "cjs" => &["javascript", "js", "jsx", "typescript", "ts", "tsx"],
            _ => &[],
        };
        for token in alias_tokens {
            if let Some(syntax) = syntax_set.find_syntax_by_token(token) {
                return syntax;
            }
        }

        let alias_names: &[&str] = match ext.as_str() {
            "ts" | "tsx" | "mts" | "cts" => &[
                "TypeScript",
                "TypeScript React",
                "JavaScript",
                "JavaScript (Babel)",
            ],
            "js" | "jsx" | "mjs" | "cjs" => &["JavaScript", "JavaScript (Babel)", "TypeScript"],
            _ => &[],
        };
        for name in alias_names {
            if let Some(syntax) = syntax_set.find_syntax_by_name(name) {
                return syntax;
            }
        }
    }

    syntax_set.find_syntax_plain_text()
}

fn syntect_color_to_iced(color: syntect::highlighting::Color) -> iced::Color {
    // Keep syntax colors fully opaque in the Iced viewer; some themes encode alpha in ways
    // that can make token colors appear washed out or invisible.
    let _ = color.a;
    iced::Color::from_rgb8(color.r, color.g, color.b)
}

fn build_syntax_highlight_lines(
    path: &Path,
    content: &str,
    is_dark_theme: bool,
) -> (Option<Vec<SyntaxHighlightLine>>, Option<String>) {
    if content.is_empty() {
        return (Some(Vec::new()), None);
    }

    let total_line_count = LinesWithEndings::from(content).count();
    let cache_key = syntax_highlight_cache_key(path, is_dark_theme, total_line_count);
    if let Some(key) = cache_key.as_ref() {
        if let Ok(mut cache) = syntax_highlight_cache().lock() {
            if let Some(entry) = cache.get(key) {
                perf_log!("syntect cache_hit path={}", path.display());
                return (entry.lines, entry.notice);
            }
        }
    }

    let total_bytes = content.len();

    let syntax_set = syntect_syntax_set();
    let syntax = syntect_syntax_for_path(path);
    let theme = syntect_theme_for(is_dark_theme);
    let mut highlighter = HighlightLines::new(syntax, theme);
    let fallback_color = if is_dark_theme {
        color!(0xcdd6f4)
    } else {
        color!(0x4c4f69)
    };

    let mut highlighted_lines =
        Vec::with_capacity(total_line_count.min(MAX_SYNTAX_HIGHLIGHT_LINES));
    let mut parse_errors = 0usize;
    let mut highlighted_line_count = 0usize;
    let mut highlighted_bytes = 0usize;
    let mut highlighted_segment_count = 0usize;
    let mut truncation_notice: Option<String> = None;

    for line_with_ending in LinesWithEndings::from(content) {
        let line_bytes = line_with_ending.len();
        if highlighted_line_count >= MAX_SYNTAX_HIGHLIGHT_LINES {
            truncation_notice = Some(format!(
                "Syntax highlighting limited to first {} lines for performance.",
                MAX_SYNTAX_HIGHLIGHT_LINES
            ));
            break;
        }
        if highlighted_bytes + line_bytes > MAX_SYNTAX_HIGHLIGHT_BYTES {
            truncation_notice = Some(format!(
                "Syntax highlighting limited to first ~{} KB for performance.",
                MAX_SYNTAX_HIGHLIGHT_BYTES / 1024
            ));
            break;
        }

        let line = line_with_ending.trim_end_matches(['\r', '\n']);
        let mut segments = Vec::new();
        if let Ok(ranges) = highlighter.highlight_line(line_with_ending, syntax_set) {
            for (style, token) in ranges {
                let token = token.trim_end_matches(['\r', '\n']);
                if token.is_empty() {
                    continue;
                }
                segments.push(SyntaxHighlightSegment {
                    text: token.to_string(),
                    color: syntect_color_to_iced(style.foreground),
                });
            }
        } else {
            parse_errors += 1;
        }

        highlighted_segment_count += segments.len();
        if highlighted_segment_count > MAX_SYNTAX_HIGHLIGHT_SEGMENTS {
            truncation_notice = Some(format!(
                "Syntax highlighting limited to first {} tokens for performance.",
                MAX_SYNTAX_HIGHLIGHT_SEGMENTS
            ));
            break;
        }

        if segments.is_empty() {
            let fallback_text = if line.is_empty() { " " } else { line };
            segments.push(SyntaxHighlightSegment {
                text: fallback_text.to_string(),
                color: fallback_color,
            });
        }

        highlighted_line_count += 1;
        highlighted_bytes += line_bytes;
        highlighted_lines.push(SyntaxHighlightLine { segments });
    }

    perf_log!(
        "syntect path={} syntax={} lines={} highlighted_lines={} bytes={} highlighted_bytes={} parse_errors={} truncated={}",
        path.display(),
        syntax.name,
        total_line_count,
        highlighted_lines.len(),
        total_bytes,
        highlighted_bytes,
        parse_errors,
        truncation_notice.is_some()
    );
    let lines = Some(highlighted_lines);
    if let Some(key) = cache_key {
        if let Ok(mut cache) = syntax_highlight_cache().lock() {
            cache.put(
                key,
                SyntaxHighlightCacheEntry {
                    lines: lines.clone(),
                    notice: truncation_notice.clone(),
                },
            );
        }
    }

    (lines, truncation_notice)
}

fn build_diff_syntax_highlight_lines_cached(
    file_path: &str,
    is_staged: bool,
    diff_lines: &[DiffLine],
    is_dark_theme: bool,
) -> (Option<Vec<Vec<SyntaxHighlightSegment>>>, Option<String>) {
    if diff_lines.is_empty() {
        return (Some(Vec::new()), None);
    }

    let cache_key = DiffSyntaxCacheKey {
        file_path: file_path.to_string(),
        is_staged,
        is_dark_theme,
        line_count: diff_lines.len(),
        content_hash: hash_diff_lines(diff_lines),
    };

    if let Ok(mut cache) = diff_syntax_cache().lock() {
        if let Some(entry) = cache.get(&cache_key) {
            perf_log!(
                "syntect diff cache_hit path={} staged={} lines={}",
                file_path,
                is_staged,
                diff_lines.len()
            );
            return (entry.lines, entry.notice);
        }
    }

    let (lines, notice) = build_diff_syntax_highlight_lines(file_path, diff_lines, is_dark_theme);
    if let Ok(mut cache) = diff_syntax_cache().lock() {
        cache.put(
            cache_key,
            DiffSyntaxCacheEntry {
                lines: lines.clone(),
                notice: notice.clone(),
            },
        );
    }

    (lines, notice)
}

fn build_diff_syntax_highlight_lines(
    file_path: &str,
    diff_lines: &[DiffLine],
    is_dark_theme: bool,
) -> (Option<Vec<Vec<SyntaxHighlightSegment>>>, Option<String>) {
    if diff_lines.is_empty() {
        return (Some(Vec::new()), None);
    }

    let approx_bytes: usize = diff_lines.iter().map(|line| line.content.len() + 1).sum();

    let syntax_set = syntect_syntax_set();
    let syntax = syntect_syntax_for_path(Path::new(file_path));
    let theme = syntect_theme_for(is_dark_theme);
    let mut highlighter = HighlightLines::new(syntax, theme);
    let fallback_color = if is_dark_theme {
        color!(0xcdd6f4)
    } else {
        color!(0x4c4f69)
    };

    let mut highlighted = Vec::with_capacity(diff_lines.len());
    let mut parse_errors = 0usize;
    let mut segment_count = 0usize;
    let mut highlighted_lines = 0usize;
    let mut highlighted_bytes = 0usize;
    let mut truncation_notice: Option<String> = None;
    let mut highlighting_budget_exhausted = false;

    for line in diff_lines {
        if line.line_type == DiffLineType::Header || highlighting_budget_exhausted {
            highlighted.push(Vec::new());
            continue;
        }

        if highlighted_lines >= MAX_DIFF_SYNTAX_HIGHLIGHT_LINES {
            truncation_notice = Some(format!(
                "Syntax highlighting limited to first {} diff lines for performance.",
                MAX_DIFF_SYNTAX_HIGHLIGHT_LINES
            ));
            highlighting_budget_exhausted = true;
            highlighted.push(Vec::new());
            continue;
        }

        let line_bytes = line.content.len() + 1;
        if highlighted_bytes + line_bytes > MAX_DIFF_SYNTAX_HIGHLIGHT_BYTES {
            truncation_notice = Some(format!(
                "Syntax highlighting limited to first ~{} KB of diff for performance.",
                MAX_DIFF_SYNTAX_HIGHLIGHT_BYTES / 1024
            ));
            highlighting_budget_exhausted = true;
            highlighted.push(Vec::new());
            continue;
        }

        let mut content_with_newline = line.content.clone();
        content_with_newline.push('\n');

        let mut segments = Vec::new();
        match highlighter.highlight_line(&content_with_newline, syntax_set) {
            Ok(ranges) => {
                for (style, token) in ranges {
                    let token = token.trim_end_matches(['\r', '\n']);
                    if token.is_empty() {
                        continue;
                    }
                    segments.push(SyntaxHighlightSegment {
                        text: token.to_string(),
                        color: syntect_color_to_iced(style.foreground),
                    });
                }
            }
            Err(_) => {
                parse_errors += 1;
            }
        }

        if segments.is_empty() {
            let fallback_text = if line.content.is_empty() {
                " "
            } else {
                &line.content
            };
            segments.push(SyntaxHighlightSegment {
                text: fallback_text.to_string(),
                color: fallback_color,
            });
        }

        segment_count += segments.len();
        if segment_count > MAX_DIFF_SYNTAX_SEGMENTS {
            truncation_notice = Some(format!(
                "Syntax highlighting limited to first {} tokens for performance.",
                MAX_DIFF_SYNTAX_SEGMENTS
            ));
            highlighting_budget_exhausted = true;
            highlighted.push(Vec::new());
            continue;
        }

        highlighted_lines += 1;
        highlighted_bytes += line_bytes;
        highlighted.push(segments);
    }

    perf_log!(
        "syntect diff path={} syntax={} lines={} highlighted_lines={} bytes={} highlighted_bytes={} parse_errors={} truncated={}",
        file_path,
        syntax.name,
        highlighted.len(),
        highlighted_lines,
        approx_bytes,
        highlighted_bytes,
        parse_errors,
        truncation_notice.is_some()
    );

    (Some(highlighted), truncation_notice)
}

// Console panel status
#[derive(Debug, Clone, Copy, PartialEq)]
enum ConsoleStatus {
    Running,
    Stopped,
    Error,
    NoneConfigured,
}

#[derive(Debug, Clone)]
struct ConsoleOutputLine {
    timestamp: String,
    content: String,
}

// Sent through mpsc channel from background task
#[derive(Debug)]
enum ConsoleOutputMessage {
    Stdout(String),
    Stderr(String),
    Exited(Option<i32>),
}

struct ConsoleState {
    run_command: Option<String>,
    status: ConsoleStatus,
    exit_code: Option<i32>,
    started_at: Option<std::time::Instant>,
    stopped_at: Option<std::time::Instant>,
    output_lines: Vec<ConsoleOutputLine>,
    output_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ConsoleOutputMessage>>,
    child_killer: Option<tokio::sync::oneshot::Sender<()>>,
    detected_url: Option<String>,
    editor_content: text_editor::Content,
    editor_dirty: bool,
    search_query: String,
    search_visible: bool,
}

impl ConsoleState {
    fn new(run_command: Option<String>) -> Self {
        let status = if run_command.is_some() {
            ConsoleStatus::Stopped
        } else {
            ConsoleStatus::NoneConfigured
        };
        Self {
            run_command,
            status,
            exit_code: None,
            started_at: None,
            stopped_at: None,
            output_lines: Vec::new(),
            output_rx: None,
            child_killer: None,
            detected_url: None,
            editor_content: text_editor::Content::new(),
            editor_dirty: false,
            search_query: String::new(),
            search_visible: false,
        }
    }

    fn push_line(&mut self, content: String, _is_stderr: bool) {
        // Detect URLs/ports in output (only if we haven't found one yet)
        if self.detected_url.is_none() {
            if let Some(url) = Self::detect_url(&content) {
                self.detected_url = Some(url);
            }
        }
        let now = chrono::Local::now();
        let timestamp = now.format("%H:%M:%S").to_string();
        self.output_lines.push(ConsoleOutputLine {
            timestamp: timestamp.clone(),
            content,
        });
        // Cap output buffer
        if self.output_lines.len() > MAX_CONSOLE_LINES {
            let drain_count = self.output_lines.len() - MAX_CONSOLE_LINES;
            self.output_lines.drain(..drain_count);
        }
        self.editor_dirty = true;
    }

    /// Rebuild editor content from output_lines if dirty. Called once per drain batch.
    fn rebuild_if_dirty(&mut self) {
        if !self.editor_dirty {
            return;
        }
        self.editor_dirty = false;
        self.rebuild_editor_content();
    }

    fn rebuild_editor_content(&mut self) {
        let query = self.search_query.to_lowercase();
        let filtering = self.search_visible && !query.is_empty();
        let full_text: String = self
            .output_lines
            .iter()
            .filter(|l| {
                !filtering
                    || l.content.to_lowercase().contains(&query)
                    || l.timestamp.contains(&query)
            })
            .map(|l| format!("{} {}", l.timestamp, l.content))
            .collect::<Vec<_>>()
            .join("\n");
        self.editor_content = text_editor::Content::with_text(&full_text);
    }

    fn matching_line_count(&self) -> usize {
        let query = self.search_query.to_lowercase();
        if query.is_empty() {
            return 0;
        }
        self.output_lines
            .iter()
            .filter(|l| l.content.to_lowercase().contains(&query) || l.timestamp.contains(&query))
            .count()
    }

    /// Strip ANSI escape sequences from a string.
    fn strip_ansi(s: &str) -> String {
        let mut result = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // Skip ESC [ ... (single letter terminator)
                if let Some(next) = chars.next() {
                    if next == '[' {
                        // Consume until we hit a letter (the terminator)
                        for tc in chars.by_ref() {
                            if tc.is_ascii_alphabetic() {
                                break;
                            }
                        }
                    }
                    // Otherwise skip just the ESC + next char
                }
            } else {
                result.push(c);
            }
        }
        result
    }

    /// Scan a line of console output for a URL or port pattern.
    fn detect_url(line: &str) -> Option<String> {
        let clean = Self::strip_ansi(line);
        // Match explicit URLs: http://localhost:3000, http://127.0.0.1:8080, etc.
        if let Some(start) = clean.find("http://") {
            let url = &clean[start..];
            let end = url
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ')' || c == ']')
                .unwrap_or(url.len());
            return Some(url[..end].to_string());
        }
        if let Some(start) = clean.find("https://localhost") {
            let url = &clean[start..];
            let end = url
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ')' || c == ']')
                .unwrap_or(url.len());
            return Some(url[..end].to_string());
        }
        // Match "listening on :3000" or "port 3000" patterns
        let lower = clean.to_lowercase();
        for pattern in &["listening on :", "port ", "on port "] {
            if let Some(pos) = lower.find(pattern) {
                let after = &clean[pos + pattern.len()..];
                let port_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(port) = port_str.parse::<u16>() {
                    if port > 0 {
                        return Some(format!("http://localhost:{}", port));
                    }
                }
            }
        }
        None
    }

    fn clear_output(&mut self) {
        self.output_lines.clear();
        self.editor_content = text_editor::Content::new();
        self.editor_dirty = false;
        self.search_query.clear();
        self.search_visible = false;
    }

    fn uptime_string(&self) -> String {
        if let Some(started) = self.started_at {
            let elapsed = if self.status == ConsoleStatus::Running {
                started.elapsed()
            } else if let Some(stopped) = self.stopped_at {
                stopped.duration_since(started)
            } else {
                started.elapsed()
            };
            let secs = elapsed.as_secs();
            if secs < 60 {
                format!("\u{2191} {}s", secs)
            } else if secs < 3600 {
                format!("\u{2191} {}m", secs / 60)
            } else {
                format!("\u{2191} {}h{}m", secs / 3600, (secs % 3600) / 60)
            }
        } else {
            String::new()
        }
    }

    fn is_running(&self) -> bool {
        self.status == ConsoleStatus::Running
    }

    fn spawn_process_with_env(&mut self, dir: &Path, extra_env: &[(String, String)]) {
        let cmd_str = match &self.run_command {
            Some(cmd) => cmd.clone(),
            None => return,
        };

        // Set up channels
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();

        self.output_rx = Some(rx);
        self.child_killer = Some(kill_tx);
        self.status = ConsoleStatus::Running;
        self.exit_code = None;
        self.started_at = Some(std::time::Instant::now());
        self.stopped_at = None;

        let dir = dir.to_path_buf();
        let extra_env = extra_env.to_vec();

        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            use tokio::process::Command;

            // Use a login shell so the user's full environment is available
            // (bun, nvm, cargo, etc. all add to PATH via shell profiles)
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());

            let mut cmd = Command::new(&shell);
            cmd.arg("-l")
                .arg("-c")
                .arg(&cmd_str)
                .current_dir(&dir)
                .env("TERM", "dumb")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());

            for (key, value) in &extra_env {
                cmd.env(key, value);
            }

            // Spawn in its own process group so we can kill the entire tree
            #[cfg(unix)]
            cmd.process_group(0);

            let mut child = match cmd.spawn() {
                Ok(child) => child,
                Err(e) => {
                    let _ = tx.send(ConsoleOutputMessage::Stderr(format!(
                        "Failed to start: {}",
                        e
                    )));
                    let _ = tx.send(ConsoleOutputMessage::Exited(Some(1)));
                    return;
                }
            };

            #[cfg(unix)]
            let child_pid = child.id();

            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();

            let tx_out = tx.clone();
            let stdout_task = tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx_out.send(ConsoleOutputMessage::Stdout(line)).is_err() {
                        break;
                    }
                }
            });

            let tx_err = tx.clone();
            let stderr_task = tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx_err.send(ConsoleOutputMessage::Stderr(line)).is_err() {
                        break;
                    }
                }
            });

            // Wait for either kill signal or natural exit
            tokio::select! {
                _ = kill_rx => {
                    // Kill the entire process group (shell + all children)
                    #[cfg(unix)]
                    if let Some(pid) = child_pid {
                        unsafe { libc::kill(-(pid as i32), libc::SIGTERM); }
                        // Give processes a moment to clean up, then force kill
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        unsafe { libc::kill(-(pid as i32), libc::SIGKILL); }
                    }
                    #[cfg(not(unix))]
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    let _ = tx.send(ConsoleOutputMessage::Exited(None));
                }
                status = child.wait() => {
                    let code = status.ok().and_then(|s| s.code());
                    let _ = tx.send(ConsoleOutputMessage::Exited(code));
                }
            }

            stdout_task.abort();
            stderr_task.abort();
        });
    }

    fn kill_process(&mut self) {
        if let Some(killer) = self.child_killer.take() {
            let _ = killer.send(());
        }
        self.stopped_at = Some(std::time::Instant::now());
        // output_rx will drain remaining messages including Exited
    }
}

fn detect_run_command(dir: &PathBuf) -> Option<String> {
    // Detect package manager (used by multiple checks)
    let detect_pm = |dir: &PathBuf| -> &str {
        if dir.join("bun.lockb").exists() || dir.join("bun.lock").exists() {
            "bun"
        } else if dir.join("yarn.lock").exists() {
            "yarn"
        } else if dir.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else {
            "npm"
        }
    };

    // 1. Tauri app (has src-tauri/ directory with Cargo.toml)
    if dir.join("src-tauri").join("Cargo.toml").exists() {
        // Check if package.json has a "tauri" script (frontend-driven setup)
        if dir.join("package.json").exists() {
            let pm = detect_pm(dir);
            if let Ok(contents) = std::fs::read_to_string(dir.join("package.json")) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents) {
                    if let Some(scripts) = json.get("scripts").and_then(|s| s.as_object()) {
                        if scripts.contains_key("tauri") {
                            return Some(format!("{} run tauri", pm));
                        }
                        if scripts.contains_key("tauri:dev") {
                            return Some(format!("{} run tauri:dev", pm));
                        }
                    }
                }
            }
        }
        // Fallback: run cargo tauri dev from inside src-tauri
        return Some("cd src-tauri && cargo tauri dev".to_string());
    }

    // 2. package.json (check before bare Cargo.toml for hybrid projects)
    if dir.join("package.json").exists() {
        let pm = detect_pm(dir);

        if let Ok(contents) = std::fs::read_to_string(dir.join("package.json")) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents) {
                if let Some(scripts) = json.get("scripts").and_then(|s| s.as_object()) {
                    // Priority: tauri dev > dev > start
                    for script_name in &["tauri dev", "dev", "start"] {
                        if scripts.contains_key(*script_name) {
                            if *script_name == "start" && pm == "npm" {
                                return Some("npm start".to_string());
                            }
                            return Some(format!("{} run {}", pm, script_name));
                        }
                    }
                }
            }
        }
    }

    // 3. Cargo.toml (plain Rust project)
    if dir.join("Cargo.toml").exists() {
        return Some("cargo run".to_string());
    }

    // 4. docker-compose
    if dir.join("docker-compose.yml").exists() || dir.join("docker-compose.yaml").exists() {
        return Some("docker compose up".to_string());
    }

    // 5. Go project
    if dir.join("go.mod").exists() && (dir.join("main.go").exists() || dir.join("cmd").is_dir()) {
        return Some("go run .".to_string());
    }

    None
}

// Tab state. Tab-kind data structures live in `src/tab/mod.rs`; the heavy `impl TabState`
// methods (load_file, fetch_status, fetch_diff, fetch_claude_config, fetch_agent_activity, etc.)
// stay here because they reference too many in-binary helpers, constants, and macros.
struct TabState {
    id: usize,
    repo_path: PathBuf,
    repo_name: String,
    kind: TabKind,
    staged: Vec<FileEntry>,
    unstaged: Vec<FileEntry>,
    untracked: Vec<FileEntry>,
    branch_name: String,
    last_poll: Instant,
    git_poll_interval_ms: u64,
    git_unchanged_streak: u32,
    last_git_status_hash: Option<u64>,
    git_status_loading: bool,
    selected_file: Option<String>,
    selected_is_staged: bool,
    diff_lines: Vec<DiffLine>,
    diff_load_in_progress: bool,
    diff_load_started_at: Option<Instant>,
    diff_syntax_lines: Option<Vec<Vec<SyntaxHighlightSegment>>>,
    diff_syntax_notice: Option<String>,
    // For keyboard navigation
    file_index: i32,
    // Track when tab was created for delayed terminal display
    created_at: Instant,
    // Sidebar mode (Git or Files)
    sidebar_mode: SidebarMode,
    // File explorer state
    current_dir: PathBuf,
    file_tree: Vec<FileTreeEntry>,
    // Search state
    search: SearchState,
    // Attention: true when terminal title starts with "*" (e.g. Claude Code waiting for input)
    needs_attention: bool,
    // Claude config tree view
    claude_config: ClaudeConfig,
    // Agent activity sidebar state (shared across tab kinds — see AgentActivityState).
    agent_sidebar: AgentActivityState,
    is_git_repo: bool,
}

impl TabState {
    fn new(id: usize, repo_path: PathBuf) -> Self {
        let repo_name = repo_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());
        let current_dir = repo_path.clone();
        let is_git_repo = Repository::discover(&repo_path).is_ok();

        Self {
            id,
            repo_path,
            repo_name,
            kind: TabKind::Terminal(TerminalTab::new()),
            staged: Vec::new(),
            unstaged: Vec::new(),
            untracked: Vec::new(),
            branch_name: String::from("main"),
            last_poll: Instant::now() - Duration::from_millis(GIT_POLL_FAST_INTERVAL_MS),
            git_poll_interval_ms: GIT_POLL_FAST_INTERVAL_MS,
            git_unchanged_streak: 0,
            last_git_status_hash: None,
            git_status_loading: false,
            selected_file: None,
            selected_is_staged: false,
            diff_lines: Vec::new(),
            diff_load_in_progress: false,
            diff_load_started_at: None,
            diff_syntax_lines: None,
            diff_syntax_notice: None,
            file_index: -1,
            created_at: Instant::now(),
            sidebar_mode: SidebarMode::Git,
            current_dir,
            file_tree: Vec::new(),
            search: SearchState::default(),
            needs_attention: false,
            claude_config: ClaudeConfig::default(),
            agent_sidebar: AgentActivityState::default(),
            is_git_repo,
        }
    }

    // ---- Tab-kind accessors -------------------------------------------------
    //
    // Agent tabs don't have a terminal, terminal title, startup command, or file
    // viewer overlay — these accessors all return None / no-op for that variant.
    // The Agent variant has its own state in `AgentSession`; see `agent_session()`.

    /// The terminal widget if this is a terminal tab and a terminal has been spawned.
    fn terminal(&self) -> Option<&iced_term::Terminal> {
        match &self.kind {
            TabKind::Terminal(tt) => tt.terminal.as_ref(),
            TabKind::Agent(_) => None,
        }
    }

    fn terminal_mut(&mut self) -> Option<&mut iced_term::Terminal> {
        match &mut self.kind {
            TabKind::Terminal(tt) => tt.terminal.as_mut(),
            TabKind::Agent(_) => None,
        }
    }

    fn set_terminal(&mut self, terminal: iced_term::Terminal) {
        match &mut self.kind {
            TabKind::Terminal(tt) => tt.terminal = Some(terminal),
            TabKind::Agent(_) => {}
        }
    }

    /// Title set by the shell via OSC escape codes — terminal tabs only.
    fn terminal_title(&self) -> Option<&str> {
        match &self.kind {
            TabKind::Terminal(tt) => tt.terminal_title.as_deref(),
            TabKind::Agent(_) => None,
        }
    }

    fn set_terminal_title(&mut self, title: Option<String>) {
        match &mut self.kind {
            TabKind::Terminal(tt) => tt.terminal_title = title,
            TabKind::Agent(_) => {}
        }
    }

    /// The startup command requested when this tab was created (e.g. "claude").
    fn startup_command(&self) -> Option<&str> {
        match &self.kind {
            TabKind::Terminal(tt) => tt.startup_command.as_deref(),
            TabKind::Agent(_) => None,
        }
    }

    fn set_startup_command(&mut self, cmd: Option<String>) {
        match &mut self.kind {
            TabKind::Terminal(tt) => tt.startup_command = cmd,
            TabKind::Agent(_) => {}
        }
    }

    /// File viewer overlay (modal-style on top of a terminal tab) if one is open.
    fn file_viewer(&self) -> Option<&FileViewerOverlay> {
        match &self.kind {
            TabKind::Terminal(tt) => tt.file_viewer.as_ref(),
            TabKind::Agent(_) => None,
        }
    }

    fn file_viewer_mut(&mut self) -> Option<&mut FileViewerOverlay> {
        match &mut self.kind {
            TabKind::Terminal(tt) => tt.file_viewer.as_mut(),
            TabKind::Agent(_) => None,
        }
    }

    /// Path of the file currently shown in the file viewer overlay, if any.
    fn viewing_file_path(&self) -> Option<&Path> {
        self.file_viewer().map(|fv| fv.path.as_path())
    }

    /// Open (or replace) a file viewer overlay on this tab. Returns `None` for
    /// agent tabs (which don't host file viewers in v1). The caller is responsible
    /// for kicking off the actual file load — this just clears prior overlay state.
    fn open_file_viewer(&mut self, path: PathBuf) -> Option<&mut FileViewerOverlay> {
        match &mut self.kind {
            TabKind::Terminal(tt) => {
                tt.file_viewer = Some(FileViewerOverlay::for_path(path));
                Some(tt.file_viewer.as_mut().expect("just inserted"))
            }
            TabKind::Agent(_) => None,
        }
    }

    fn close_file_viewer(&mut self) {
        match &mut self.kind {
            TabKind::Terminal(tt) => tt.file_viewer = None,
            TabKind::Agent(_) => {}
        }
    }

    fn last_view_request(&self) -> Option<(&Path, Instant)> {
        match &self.kind {
            TabKind::Terminal(tt) => tt
                .last_view_request_path
                .as_deref()
                .zip(tt.last_view_request_at),
            TabKind::Agent(_) => None,
        }
    }

    fn set_last_view_request(&mut self, path: Option<PathBuf>, at: Option<Instant>) {
        match &mut self.kind {
            TabKind::Terminal(tt) => {
                tt.last_view_request_path = path;
                tt.last_view_request_at = at;
            }
            TabKind::Agent(_) => {}
        }
    }

    /// The agent session for an agent tab, if this is one.
    #[allow(dead_code)] // Used in Step 3+
    fn agent_session(&self) -> Option<&AgentSession> {
        match &self.kind {
            TabKind::Terminal(_) => None,
            TabKind::Agent(s) => Some(s),
        }
    }

    #[allow(dead_code)] // Used in Step 3+
    fn agent_session_mut(&mut self) -> Option<&mut AgentSession> {
        match &mut self.kind {
            TabKind::Terminal(_) => None,
            TabKind::Agent(s) => Some(s),
        }
    }

    fn is_image_file(path: &Path) -> bool {
        path.extension()
            .and_then(|e: &std::ffi::OsStr| e.to_str())
            .map(|ext: &str| {
                matches!(
                    ext.to_lowercase().as_str(),
                    "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico"
                )
            })
            .unwrap_or(false)
    }

    fn is_markdown_file(path: &Path) -> bool {
        path.extension()
            .and_then(|e: &std::ffi::OsStr| e.to_str())
            .map(|ext: &str| matches!(ext.to_lowercase().as_str(), "md" | "markdown"))
            .unwrap_or(false)
    }

    fn is_html_file(path: &Path) -> bool {
        path.extension()
            .and_then(|e: &std::ffi::OsStr| e.to_str())
            .map(|ext: &str| matches!(ext.to_lowercase().as_str(), "html" | "htm"))
            .unwrap_or(false)
    }

    /// Try to extract a directory path from the terminal title.
    /// Handles common shell title formats:
    /// - "~/path" or "/absolute/path"
    /// - "~/path (extra)" - path followed by parenthetical info
    /// - "dirname — zsh" (starship style)
    /// - "user@host:~/path" (standard zsh/bash)
    fn extract_dir_from_title(title: &str) -> Option<PathBuf> {
        let home = std::env::var("HOME").ok()?;

        // Helper to expand ~ and check if path exists
        let try_path = |s: &str| -> Option<PathBuf> {
            let s = s.trim();
            let expanded = if let Some(rest) = s.strip_prefix("~/") {
                format!("{}/{}", home, rest)
            } else if s == "~" {
                home.clone()
            } else if s.starts_with('/') {
                s.to_string()
            } else {
                return None;
            };
            let path = PathBuf::from(&expanded);
            if path.is_dir() {
                Some(path)
            } else {
                None
            }
        };

        // First: look for path at the start, ending at " (" which often indicates extra info
        // e.g., "~/GitRepo/project (18) ⌘1" -> extract "~/GitRepo/project"
        if let Some(paren_pos) = title.find(" (") {
            let candidate = &title[..paren_pos];
            if let Some(path) = try_path(candidate) {
                return Some(path);
            }
        }

        // Second: try the whole title as-is
        if let Some(path) = try_path(title) {
            return Some(path);
        }

        // Third: split by em-dash or colon (but NOT hyphen, as paths often have hyphens)
        for sep in &['\u{2014}', ':'] {
            // em-dash, colon
            for part in title.split(*sep) {
                // Also try stripping at " (" within each part
                let part = if let Some(pos) = part.find(" (") {
                    &part[..pos]
                } else {
                    part
                };
                if let Some(path) = try_path(part) {
                    return Some(path);
                }
            }
        }

        None
    }

    #[allow(dead_code)]
    fn fetch_file_tree(&mut self, _show_hidden: bool) {
        self.file_tree.clear();

        if let Ok(entries) = std::fs::read_dir(&self.current_dir) {
            let mut dirs: Vec<FileTreeEntry> = Vec::new();
            let mut files: Vec<FileTreeEntry> = Vec::new();

            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();

                // Always skip node_modules and target
                if name == "node_modules" || name == "target" {
                    continue;
                }

                let is_dir = path.is_dir();
                let entry = FileTreeEntry { name, path, is_dir };

                if is_dir {
                    dirs.push(entry);
                } else {
                    files.push(entry);
                }
            }

            // Sort alphabetically
            dirs.sort_by_key(|a| a.name.to_lowercase());
            files.sort_by_key(|a| a.name.to_lowercase());

            // Dirs first, then files
            self.file_tree.extend(dirs);
            self.file_tree.extend(files);
        }
    }

    #[allow(dead_code)]
    fn load_file(&mut self, path: &PathBuf, is_dark_theme: bool) {
        // Reset / open the overlay so subsequent writes target a fresh state.
        // No-op for agent tabs (which don't host file viewers in v1).
        let Some(fv) = self.open_file_viewer(path.clone()) else {
            return;
        };

        let file_size = freeze_time!("file metadata check", {
            std::fs::metadata(path).ok().map(|m| m.len()).unwrap_or(0)
        });

        // Warn about potentially problematic files
        if file_size > 1_000_000 {
            // 1MB
            freeze_debug!(
                "Loading large file ({} bytes): {}",
                file_size,
                path.display()
            );
        }

        // Refuse to load extremely large files synchronously
        if file_size > 50_000_000 {
            // 50MB
            fv.preview_notice = Some(format!(
                "File too large ({}) for inline preview. Click \"View in Browser\" to open externally.",
                format_bytes(file_size)
            ));
            return;
        }

        #[cfg(feature = "excalidraw")]
        if excalidraw::is_excalidraw_file(path) {
            if file_size > MAX_INLINE_WEBVIEW_BYTES {
                fv.preview_notice = Some(format!(
                    "Inline preview skipped for large Excalidraw file ({}). Click \"View in Browser\".",
                    format_bytes(file_size)
                ));
                return;
            }
            let content_result =
                freeze_time!("excalidraw file read", { std::fs::read_to_string(path) });
            if let Ok(content) = content_result {
                if excalidraw::validate_excalidraw(&content) {
                    let html = excalidraw::render_excalidraw_html(&content, is_dark_theme);
                    fv.webview_content = Some(html);
                }
            }
            return;
        }

        if Self::is_markdown_file(path) {
            if file_size > MAX_INLINE_WEBVIEW_BYTES {
                fv.preview_notice = Some(format!(
                    "Inline preview skipped for large Markdown file ({}). Click \"View in Browser\".",
                    format_bytes(file_size)
                ));
                return;
            }
            // Load as markdown - render to HTML and store for potential browser viewing
            let content_result =
                freeze_time!("markdown file read", { std::fs::read_to_string(path) });
            if let Ok(content) = content_result {
                let html = markdown::render_markdown_to_html(&content, is_dark_theme);
                fv.webview_content = Some(html);
            }
        } else if Self::is_html_file(path) {
            if file_size > MAX_INLINE_WEBVIEW_BYTES {
                fv.preview_notice = Some(format!(
                    "Inline preview skipped for large HTML file ({}). Click \"View in Browser\".",
                    format_bytes(file_size)
                ));
                return;
            }
            let content_result = freeze_time!("HTML file read", { std::fs::read_to_string(path) });
            if let Ok(content) = content_result {
                fv.webview_content = Some(content);
            }
        } else if Self::is_image_file(path) {
            // Load as image
            fv.image_handle = Some(image::Handle::from_path(path));
        } else if file_size > MAX_FULL_TEXT_LOAD_BYTES {
            if let Ok(preview) =
                read_text_preview(path, LARGE_TEXT_PREVIEW_BYTES, LARGE_TEXT_PREVIEW_LINES)
            {
                fv.file_content = preview;
            } else if let Ok(content) = std::fs::read_to_string(path) {
                fv.file_content = content;
            }
            fv.preview_notice = Some(format!(
                "Large file ({}): showing first {} lines (~{} KB).",
                format_bytes(file_size),
                LARGE_TEXT_PREVIEW_LINES,
                LARGE_TEXT_PREVIEW_BYTES / 1024
            ));
        } else {
            // For large files, show a preview only
            if file_size > 5_000_000 {
                // 5MB
                let preview_result = freeze_time!("large text file preview", {
                    read_text_preview(path, LARGE_TEXT_PREVIEW_BYTES, LARGE_TEXT_PREVIEW_LINES)
                });
                match preview_result {
                    Ok(preview) => {
                        fv.file_content = preview;
                        fv.preview_notice = Some(format!(
                            "Large file ({}): showing preview only. Click \"View in Browser\" for full content.",
                            format_bytes(file_size)
                        ));
                    }
                    Err(_) => {
                        fv.preview_notice = Some(format!(
                            "Could not load file preview ({})",
                            format_bytes(file_size)
                        ));
                    }
                }
            } else {
                let content_result =
                    freeze_time!("text file read", { std::fs::read_to_string(path) });
                if let Ok(content) = content_result {
                    // Load as text
                    fv.file_content = content;
                }
            }
        }

        if fv.webview_content.is_none() && fv.image_handle.is_none() && !fv.file_content.is_empty()
        {
            let (lines, notice) =
                build_syntax_highlight_lines(path, &fv.file_content, is_dark_theme);
            fv.syntax_highlight_lines = lines;
            fv.syntax_highlight_notice = notice;
        }
    }

    fn total_changes(&self) -> usize {
        self.staged.len() + self.unstaged.len() + self.untracked.len()
    }

    fn all_files(&self) -> Vec<&FileEntry> {
        self.staged
            .iter()
            .chain(self.unstaged.iter())
            .chain(self.untracked.iter())
            .collect()
    }

    #[allow(dead_code)]
    fn fetch_status(&mut self) {
        if let Ok(repo) = Repository::open(&self.repo_path) {
            // Get branch name
            if let Ok(head) = repo.head() {
                if let Some(name) = head.shorthand() {
                    self.branch_name = name.to_string();
                }
            }

            // Get file statuses
            let mut opts = StatusOptions::new();
            opts.no_refresh(true)
                .include_untracked(true)
                .recurse_untracked_dirs(true)
                .include_ignored(false);

            self.staged.clear();
            self.unstaged.clear();
            self.untracked.clear();

            if let Ok(statuses) = repo.statuses(Some(&mut opts)) {
                for entry in statuses.iter() {
                    let path = entry.path().unwrap_or("").to_string();
                    let status = entry.status();

                    if status.contains(Status::INDEX_NEW)
                        || status.contains(Status::INDEX_MODIFIED)
                        || status.contains(Status::INDEX_DELETED)
                        || status.contains(Status::INDEX_RENAMED)
                    {
                        self.staged.push(FileEntry {
                            path: path.clone(),
                            status: status_char(status, true),
                            is_staged: true,
                        });
                    }

                    if status.contains(Status::WT_MODIFIED)
                        || status.contains(Status::WT_DELETED)
                        || status.contains(Status::WT_RENAMED)
                    {
                        self.unstaged.push(FileEntry {
                            path: path.clone(),
                            status: status_char(status, false),
                            is_staged: false,
                        });
                    }

                    if status.contains(Status::WT_NEW) {
                        self.untracked.push(FileEntry {
                            path,
                            status: "?".to_string(),
                            is_staged: false,
                        });
                    }
                }
            }
        }
        self.last_poll = Instant::now();
    }

    fn fetch_agent_activity(&mut self) -> Task<Event> {
        self.agent_sidebar.loading = true;
        let tab_id = self.id;
        let repo_path = self.repo_path.clone();
        Task::perform(
            async move {
                match tokio::task::spawn_blocking(move || {
                    agent::AgentActivity::load_from_repo(&repo_path)
                })
                .await
                {
                    Ok(result) => (tab_id, result),
                    Err(e) => (tab_id, Err(format!("spawn_blocking failed: {}", e))),
                }
            },
            |(tab_id, result)| Event::AgentActivityLoaded(tab_id, result),
        )
    }

    fn fetch_claude_config(&mut self) {
        let home = dirs::home_dir().unwrap_or_default();
        let claude_home = home.join(".claude");
        let workspace_dir = &self.repo_path;

        self.claude_config.skills.clear();
        self.claude_config.plugins.clear();
        self.claude_config.mcp_servers.clear();
        self.claude_config.hooks.clear();
        self.claude_config.settings.clear();

        // --- Skills ---
        // User global skills
        let user_commands_dir = claude_home.join("commands");
        let mut skill_names: HashSet<String> = HashSet::new();

        // Project skills first (they override user skills with same name)
        let project_commands_dir = workspace_dir.join(".claude").join("commands");
        if let Ok(entries) = std::fs::read_dir(&project_commands_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    let name = path
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    skill_names.insert(name.clone());
                    self.claude_config.skills.push(ClaudeConfigItem {
                        name,
                        file_path: path,
                        scope: ConfigScope::Project,
                    });
                }
            }
        }

        if let Ok(entries) = std::fs::read_dir(&user_commands_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    let name = path
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    if !skill_names.contains(&name) {
                        self.claude_config.skills.push(ClaudeConfigItem {
                            name,
                            file_path: path,
                            scope: ConfigScope::User,
                        });
                    }
                }
            }
        }
        self.claude_config
            .skills
            .sort_by_key(|a| a.name.to_lowercase());

        // --- Read settings.json ---
        let settings_path = claude_home.join("settings.json");
        let settings_json: Option<serde_json::Value> = std::fs::read_to_string(&settings_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());

        // --- Plugins ---
        if let Some(ref settings) = settings_json {
            if let Some(plugins) = settings.get("enabledPlugins").and_then(|v| v.as_object()) {
                for key in plugins.keys() {
                    self.claude_config.plugins.push(ClaudeConfigItem {
                        name: key.clone(),
                        file_path: settings_path.clone(),
                        scope: ConfigScope::User,
                    });
                }
            }
        }
        self.claude_config
            .plugins
            .sort_by_key(|a| a.name.to_lowercase());

        // --- MCP Servers ---
        // Project .mcp.json
        let project_mcp = workspace_dir.join(".mcp.json");
        if let Ok(content) = std::fs::read_to_string(&project_mcp) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(servers) = json.get("mcpServers").and_then(|v| v.as_object()) {
                    for key in servers.keys() {
                        self.claude_config.mcp_servers.push(ClaudeConfigItem {
                            name: key.clone(),
                            file_path: project_mcp.clone(),
                            scope: ConfigScope::Project,
                        });
                    }
                }
            }
        }

        // Global ~/.claude/.mcp.json
        let global_mcp = claude_home.join(".mcp.json");
        if let Ok(content) = std::fs::read_to_string(&global_mcp) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(servers) = json.get("mcpServers").and_then(|v| v.as_object()) {
                    for key in servers.keys() {
                        self.claude_config.mcp_servers.push(ClaudeConfigItem {
                            name: key.clone(),
                            file_path: global_mcp.clone(),
                            scope: ConfigScope::User,
                        });
                    }
                }
            }
        }
        self.claude_config
            .mcp_servers
            .sort_by_key(|a| a.name.to_lowercase());

        // --- Hooks ---
        if let Some(ref settings) = settings_json {
            if let Some(hooks) = settings.get("hooks").and_then(|v| v.as_object()) {
                for key in hooks.keys() {
                    self.claude_config.hooks.push(ClaudeConfigItem {
                        name: key.clone(),
                        file_path: settings_path.clone(),
                        scope: ConfigScope::User,
                    });
                }
            }
        }
        self.claude_config
            .hooks
            .sort_by_key(|a| a.name.to_lowercase());

        // --- Settings ---
        // User settings (top-level keys, excluding plugins/hooks which have their own sections)
        if let Some(ref settings) = settings_json {
            if let Some(obj) = settings.as_object() {
                let excluded = ["enabledPlugins", "hooks"];
                for key in obj.keys() {
                    if !excluded.contains(&key.as_str()) {
                        self.claude_config.settings.push(ClaudeConfigItem {
                            name: key.clone(),
                            file_path: settings_path.clone(),
                            scope: ConfigScope::User,
                        });
                    }
                }
            }
        }

        // Project settings
        let project_settings_path = workspace_dir.join(".claude").join("settings.local.json");
        if let Ok(content) = std::fs::read_to_string(&project_settings_path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(obj) = json.as_object() {
                    for key in obj.keys() {
                        self.claude_config.settings.push(ClaudeConfigItem {
                            name: key.clone(),
                            file_path: project_settings_path.clone(),
                            scope: ConfigScope::Project,
                        });
                    }
                }
            }
        }
        self.claude_config
            .settings
            .sort_by_key(|a| a.name.to_lowercase());
    }

    #[allow(dead_code)]
    fn fetch_diff(&mut self, file_path: &str, staged: bool) {
        self.diff_lines.clear();
        self.diff_syntax_lines = None;
        self.diff_syntax_notice = None;

        let Ok(repo) = Repository::open(&self.repo_path) else {
            return;
        };

        // Check if it's untracked
        let is_untracked = self.untracked.iter().any(|f| f.path == file_path);

        if is_untracked {
            // Read file content for untracked files
            let full_path = self.repo_path.join(file_path);
            if let Ok(content) = std::fs::read_to_string(&full_path) {
                self.diff_lines.push(DiffLine {
                    content: format!("@@ -0,0 +1,{} @@ (new file)", content.lines().count()),
                    line_type: DiffLineType::Header,
                    old_line_num: None,
                    new_line_num: None,
                    inline_changes: None,
                });
                for (i, line) in content.lines().enumerate() {
                    self.diff_lines.push(DiffLine {
                        content: line.to_string(),
                        line_type: DiffLineType::Addition,
                        old_line_num: None,
                        new_line_num: Some((i + 1) as u32),
                        inline_changes: None,
                    });
                }
            }
            return;
        }

        let mut diff_opts = DiffOptions::new();
        diff_opts.pathspec(file_path);

        let diff = if staged {
            let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
            repo.diff_tree_to_index(head_tree.as_ref(), None, Some(&mut diff_opts))
        } else {
            repo.diff_index_to_workdir(None, Some(&mut diff_opts))
        };

        if let Ok(diff) = diff {
            let _ = diff.print(git2::DiffFormat::Patch, |_delta, hunk, line| {
                let content = String::from_utf8_lossy(line.content()).to_string();
                let content = content.trim_end().to_string();

                match line.origin() {
                    'H' => {
                        if let Some(h) = hunk {
                            self.diff_lines.push(DiffLine {
                                content: format!(
                                    "@@ -{},{} +{},{} @@",
                                    h.old_start(),
                                    h.old_lines(),
                                    h.new_start(),
                                    h.new_lines()
                                ),
                                line_type: DiffLineType::Header,
                                old_line_num: None,
                                new_line_num: None,
                                inline_changes: None,
                            });
                        }
                    }
                    '+' => {
                        self.diff_lines.push(DiffLine {
                            content,
                            line_type: DiffLineType::Addition,
                            old_line_num: None,
                            new_line_num: line.new_lineno(),
                            inline_changes: None,
                        });
                    }
                    '-' => {
                        self.diff_lines.push(DiffLine {
                            content,
                            line_type: DiffLineType::Deletion,
                            old_line_num: line.old_lineno(),
                            new_line_num: None,
                            inline_changes: None,
                        });
                    }
                    ' ' => {
                        self.diff_lines.push(DiffLine {
                            content,
                            line_type: DiffLineType::Context,
                            old_line_num: line.old_lineno(),
                            new_line_num: line.new_lineno(),
                            inline_changes: None,
                        });
                    }
                    _ => {}
                }
                true
            });

            // Post-process to add word-level diffs
            self.add_word_diffs();
        }
    }

    #[allow(dead_code)]
    fn add_word_diffs(&mut self) {
        let mut i = 0;
        while i < self.diff_lines.len() {
            if self.diff_lines[i].line_type == DiffLineType::Deletion {
                // Count consecutive deletions
                let mut del_end = i + 1;
                while del_end < self.diff_lines.len()
                    && self.diff_lines[del_end].line_type == DiffLineType::Deletion
                {
                    del_end += 1;
                }

                // Count consecutive additions after deletions
                let mut add_end = del_end;
                while add_end < self.diff_lines.len()
                    && self.diff_lines[add_end].line_type == DiffLineType::Addition
                {
                    add_end += 1;
                }

                let del_count = del_end - i;
                let add_count = add_end - del_end;

                // Pair up deletions with additions
                let pairs = del_count.min(add_count);
                for j in 0..pairs {
                    let del_idx = i + j;
                    let add_idx = del_end + j;

                    let del_content = self.diff_lines[del_idx].content.clone();
                    let add_content = self.diff_lines[add_idx].content.clone();

                    let word_changes = compute_word_diff(&del_content, &add_content);

                    // Check if there's meaningful overlap
                    let has_equal = word_changes
                        .iter()
                        .any(|c| c.change_type == ChangeType::Equal);

                    if has_equal {
                        // Build inline changes for deletion line
                        let del_inline: Vec<InlineChange> = word_changes
                            .iter()
                            .filter(|c| {
                                c.change_type == ChangeType::Equal
                                    || c.change_type == ChangeType::Delete
                            })
                            .cloned()
                            .collect();

                        // Build inline changes for addition line
                        let add_inline: Vec<InlineChange> = word_changes
                            .iter()
                            .filter(|c| {
                                c.change_type == ChangeType::Equal
                                    || c.change_type == ChangeType::Insert
                            })
                            .cloned()
                            .collect();

                        self.diff_lines[del_idx].inline_changes = Some(del_inline);
                        self.diff_lines[add_idx].inline_changes = Some(add_inline);
                    }
                }

                i = add_end;
            } else {
                i += 1;
            }
        }
    }
}

// Bottom panel tab types
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BottomPanelTab {
    Console,
    Terminal(usize), // index into bottom_terminals vec
}

struct BottomTerminal {
    id: usize,
    terminal: Option<iced_term::Terminal>,
    title: Option<String>,
    cwd: PathBuf,
}

// Workspace groups tabs by project
struct Workspace {
    name: String,
    abbrev: String,
    dir: PathBuf,
    color: WorkspaceColor,
    tabs: Vec<TabState>,
    active_tab: usize,
    console: ConsoleState,
    bottom_terminals: Vec<BottomTerminal>,
    active_bottom_tab: BottomPanelTab,
    env: std::collections::HashMap<String, String>,
}

impl Workspace {
    fn new(name: String, dir: PathBuf, color: WorkspaceColor) -> Self {
        let abbrev = Self::derive_abbrev(&name);
        let run_command = detect_run_command(&dir);
        let console = ConsoleState::new(run_command);
        Self {
            name,
            abbrev,
            dir,
            color,
            tabs: Vec::new(),
            active_tab: 0,
            console,
            bottom_terminals: Vec::new(),
            active_bottom_tab: BottomPanelTab::Console,
            env: std::collections::HashMap::new(),
        }
    }

    fn derive_abbrev(name: &str) -> String {
        name.chars().take(2).collect::<String>().to_uppercase()
    }

    fn active_tab(&self) -> Option<&TabState> {
        self.tabs.get(self.active_tab)
    }

    fn active_tab_mut(&mut self) -> Option<&mut TabState> {
        self.tabs.get_mut(self.active_tab)
    }

    fn attention_count(&self) -> usize {
        self.tabs.iter().filter(|t| t.needs_attention).count()
    }

    fn has_attention(&self) -> bool {
        self.tabs.iter().any(|t| t.needs_attention)
    }
}

fn compute_word_diff(old_text: &str, new_text: &str) -> Vec<InlineChange> {
    let diff = TextDiff::from_words(old_text, new_text);
    diff.iter_all_changes()
        .map(|change| {
            let change_type = match change.tag() {
                ChangeTag::Equal => ChangeType::Equal,
                ChangeTag::Insert => ChangeType::Insert,
                ChangeTag::Delete => ChangeType::Delete,
            };
            InlineChange {
                change_type,
                value: change.value().to_string(),
            }
        })
        .collect()
}

fn status_char(status: Status, staged: bool) -> String {
    if staged {
        if status.contains(Status::INDEX_NEW) {
            "A".to_string()
        } else if status.contains(Status::INDEX_MODIFIED) {
            "M".to_string()
        } else if status.contains(Status::INDEX_DELETED) {
            "D".to_string()
        } else if status.contains(Status::INDEX_RENAMED) {
            "R".to_string()
        } else {
            "?".to_string()
        }
    } else if status.contains(Status::WT_MODIFIED) {
        "M".to_string()
    } else if status.contains(Status::WT_DELETED) {
        "D".to_string()
    } else if status.contains(Status::WT_RENAMED) {
        "R".to_string()
    } else {
        "?".to_string()
    }
}

fn hash_file_entry_list(entries: &[FileEntry], hasher: &mut DefaultHasher) {
    for entry in entries {
        entry.path.hash(hasher);
        entry.status.hash(hasher);
        entry.is_staged.hash(hasher);
    }
}

fn git_tab_state_hash(tab: &TabState) -> u64 {
    let mut hasher = DefaultHasher::new();
    tab.is_git_repo.hash(&mut hasher);
    tab.branch_name.hash(&mut hasher);
    hash_file_entry_list(&tab.staged, &mut hasher);
    hash_file_entry_list(&tab.unstaged, &mut hasher);
    hash_file_entry_list(&tab.untracked, &mut hasher);
    hasher.finish()
}

fn next_git_poll_interval_ms(is_git_repo: bool, has_changes: bool, unchanged_streak: u32) -> u64 {
    if !is_git_repo {
        return GIT_POLL_NON_REPO_INTERVAL_MS;
    }

    if has_changes {
        return match unchanged_streak {
            0..=2 => GIT_POLL_FAST_INTERVAL_MS,
            3..=8 => GIT_POLL_MEDIUM_INTERVAL_MS,
            _ => GIT_POLL_SLOW_INTERVAL_MS,
        };
    }

    match unchanged_streak {
        0..=1 => GIT_POLL_FAST_INTERVAL_MS,
        2..=4 => GIT_POLL_MEDIUM_INTERVAL_MS,
        5..=8 => GIT_POLL_SLOW_INTERVAL_MS,
        _ => GIT_POLL_IDLE_INTERVAL_MS,
    }
}

fn collect_git_status(tab_id: usize, repo_path: PathBuf) -> GitStatusSnapshot {
    services::collect_git_status(tab_id, repo_path)
}

fn collect_file_tree(tab_id: usize, current_dir: PathBuf, show_hidden: bool) -> FileTreeSnapshot {
    services::collect_file_tree(tab_id, current_dir, show_hidden)
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

fn read_text_preview(path: &Path, max_bytes: usize, max_lines: usize) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; max_bytes];
    let bytes_read = file.read(&mut buf)?;
    buf.truncate(bytes_read);

    let text = String::from_utf8_lossy(&buf);
    let mut out = String::new();
    for (idx, line) in text.lines().take(max_lines).enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }

    Ok(out)
}

fn add_word_diffs_to_lines(diff_lines: &mut [DiffLine]) {
    let mut i = 0;
    while i < diff_lines.len() {
        if diff_lines[i].line_type == DiffLineType::Deletion {
            let mut del_end = i + 1;
            while del_end < diff_lines.len()
                && diff_lines[del_end].line_type == DiffLineType::Deletion
            {
                del_end += 1;
            }

            let mut add_end = del_end;
            while add_end < diff_lines.len()
                && diff_lines[add_end].line_type == DiffLineType::Addition
            {
                add_end += 1;
            }

            let pairs = (del_end - i).min(add_end - del_end);
            for j in 0..pairs {
                let del_idx = i + j;
                let add_idx = del_end + j;

                let del_content = diff_lines[del_idx].content.clone();
                let add_content = diff_lines[add_idx].content.clone();
                let word_changes = compute_word_diff(&del_content, &add_content);
                let has_equal = word_changes
                    .iter()
                    .any(|c| c.change_type == ChangeType::Equal);

                if has_equal {
                    diff_lines[del_idx].inline_changes = Some(
                        word_changes
                            .iter()
                            .filter(|c| {
                                c.change_type == ChangeType::Equal
                                    || c.change_type == ChangeType::Delete
                            })
                            .cloned()
                            .collect(),
                    );
                    diff_lines[add_idx].inline_changes = Some(
                        word_changes
                            .iter()
                            .filter(|c| {
                                c.change_type == ChangeType::Equal
                                    || c.change_type == ChangeType::Insert
                            })
                            .cloned()
                            .collect(),
                    );
                }
            }

            i = add_end;
        } else {
            i += 1;
        }
    }
}

fn collect_diff(
    tab_id: usize,
    repo_path: PathBuf,
    file_path: String,
    is_staged: bool,
) -> DiffSnapshot {
    services::collect_diff(tab_id, repo_path, file_path, is_staged)
}

fn collect_file_load(tab_id: usize, path: PathBuf, is_dark_theme: bool) -> FileLoadSnapshot {
    services::collect_file_load(tab_id, path, is_dark_theme)
}

fn collect_file_syntax_highlight(
    tab_id: usize,
    path: PathBuf,
    file_content: String,
    is_dark_theme: bool,
    file_signature: Option<FileVersionSignature>,
    max_lines: usize,
) -> FileSyntaxSnapshot {
    services::collect_file_syntax_highlight(
        tab_id,
        path,
        file_content,
        is_dark_theme,
        file_signature,
        max_lines,
    )
}

#[derive(Debug, Clone)]
pub enum Event {
    Terminal(usize, iced_term::Event),
    Tick,
    InitMenu,
    CheckMenu,
    TabSelect(usize),
    TabClose(usize),
    OpenFolder,
    FolderSelected(Option<PathBuf>),
    FileSelect(String, bool),
    FileSelectByIndex(i32),
    ClearSelection,
    KeyPressed(Key, Modifiers),
    // Sidebar
    ToggleSidebar,
    SetSidebarMode(SidebarMode),
    NavigateDir(PathBuf),
    NavigateUp,
    ViewFile(PathBuf),
    CloseFileView,
    CopyFileContent,
    OpenFileInBrowser,
    // Theme
    ToggleTheme,
    ToggleLogServer,
    // Font size - Terminal
    IncreaseTerminalFont,
    DecreaseTerminalFont,
    ClearTerminal,
    // Font size - UI
    IncreaseUiFont,
    DecreaseUiFont,
    // Hidden files
    ToggleHidden,
    // Divider dragging
    DividerDragStart,
    DividerDragEnd,
    MouseMoved(f32, f32),
    // Search events
    ToggleSearch,
    SearchQueryChanged(String),
    SearchExecute,
    SearchNext,
    SearchPrev,
    SearchClose,
    // Markdown preview
    OpenMarkdownInBrowser,
    // Plans viewer (in-app webview, served by warp on localhost)
    OpenPlansViewer,
    OpenPlan(plans_viewer::DocumentSource, String),
    ClosePlansViewer,
    // Window events
    WindowResized(f32, f32),
    WindowCloseRequested,
    WindowFocused,
    WindowUnfocused,
    // Workspace events
    WorkspaceSelect(usize),
    WorkspaceClose(usize),
    WorkspaceCreate,
    WorkspaceCreated(Option<PathBuf>),
    WorkspaceSettingsOpen(usize),
    WorkspaceSettingsClose,
    WorkspaceSettingsNewKeyChanged(String),
    WorkspaceSettingsNewValueChanged(String),
    WorkspaceSettingsEnvAdd,
    WorkspaceSettingsEnvRemove(String),
    WorkspaceSettingsColorChange(WorkspaceColor),
    WorkspaceSettingsApplyProfile(String),
    // Slide animation events
    SlideAnimationTick,
    // Edge peek events
    EdgePeekEnter(bool), // true=right, false=left
    EdgePeekExit,
    SlideScrolled(scrollable::Viewport),
    // Console panel events
    ConsoleToggle,
    ConsoleStart,
    ConsoleStop,
    ConsoleRestart,
    ConsoleClearOutput,
    ConsoleOpenBrowser,
    ConsoleDividerDragStart,
    ConsoleCommandEditStart,
    ConsoleCommandChanged(String),
    ConsoleCommandSubmit,
    ConsoleCommandCancel,
    // Attention system events
    AttentionPulseTick,
    AttentionJumpNext,
    // Launch agent preset by index
    AgentActivityLoaded(usize, Result<agent::AgentActivity, String>),
    AgentConversationLoaded(usize, agent::Conversation),
    SelectAgentCapture(usize),
    LaunchAgentPreset(usize),
    // Resume agent preset session by index
    ResumeAgentPreset(usize),
    // Live agent tab (TabKind::Agent) — Step 3 of TRU-29.
    /// User submitted a prompt for the agent tab with this id.
    AgentSubmitPrompt(usize, String),
    /// User clicked the stop button on the agent tab with this id.
    AgentStopRequested(usize),
    /// One streaming event from the agent subprocess (Step 4 will refine the
    /// payload once the parser lands; today every line arrives as `Other`).
    AgentEventReceived(usize, tab::AgentEvent),
    /// Step 3-only debug entry point: spawn an in-memory agent tab in the
    /// active workspace, submit a hardcoded prompt, log events to stderr.
    /// Bound to a hidden keyboard shortcut for verifying the subprocess
    /// pipeline. Step 4 keeps it as the temporary launcher for an agent tab
    /// (replaced by a real launcher UI in Step 5).
    DebugSpawnAgentTab,
    /// One IPC message from the agent webview, drained from the global
    /// `AGENT_IPC_RX` channel by the subscription bridge.
    AgentWebviewIpc(AgentIpcMessage),
    // Quick commands (run in bottom terminal)
    RunQuickCommand(usize),
    ShowQuickCommands,
    HideQuickCommands,
    // Plain terminal tab (no startup command)
    NewPlainTab,
    // Tab picker popup
    ShowTabPicker,
    HideTabPicker,
    // Edit file in editor
    EditFile(PathBuf),
    // Claude sidebar events
    ToggleClaudeSection(String),
    ClaudeItemSelect(String, usize),
    // Bottom panel tabs
    BottomTabSelect(BottomPanelTab),
    BottomTerminalAdd,
    BottomTerminalClose(usize),
    BottomTerminalEvent(usize, iced_term::Event),
    // Console editor (selectable output)
    ConsoleEditorAction(text_editor::Action),
    // Console search
    ConsoleSearchToggle,
    ConsoleSearchChanged(String),
    ConsoleSearchClose,
    // Modifier tracking
    ModifiersChanged(Modifiers),
    // Help modal
    ToggleHelp,
    // Terminal focus click events
    MainTerminalClicked,
    BottomTerminalClicked(usize),
    GitStatusLoaded(GitStatusSnapshot),
    FileTreeLoaded(FileTreeSnapshot),
    DiffLoaded(DiffSnapshot),
    FileLoaded(FileLoadSnapshot),
    FileViewScrolled(usize, scrollable::Viewport),
    FileSyntaxHighlighted(FileSyntaxSnapshot),
    LogServerSyncComplete,
    SyntectWarmupComplete,
    LoadingUiTick,
    // Speech-to-text events
    #[cfg(feature = "stt")]
    SttToggle,
    #[cfg(feature = "stt")]
    SttTranscriptReady(String),
    #[cfg(feature = "stt")]
    SttError(String),
}

struct App {
    title: String,
    workspaces: Vec<Workspace>,
    active_workspace_idx: usize,
    next_tab_id: usize,
    theme: AppTheme,
    terminal_font_size: f32,
    ui_font_size: f32,
    sidebar_width: f32,
    scrollback_lines: usize,
    sidebar_collapsed: bool,
    dragging_divider: bool,
    show_hidden: bool,
    window_size: (f32, f32),
    log_server_state: log_server::ServerState,
    log_server_enabled: bool,
    console_expanded: bool,
    console_height: f32,
    dragging_console_divider: bool,
    editing_console_command: Option<String>,
    // Slide animation state
    slide_offset: f32,
    slide_target: f32,
    slide_animating: bool,
    slide_start_time: Option<Instant>,
    slide_start_offset: f32,
    // User scroll tracking (for swipe debounce)
    last_user_scroll: Option<Instant>,
    // Edge peek state
    edge_peek_left: bool,
    edge_peek_right: bool,
    // Attention pulse animation (toggles every 500ms)
    attention_pulse_bright: bool,
    // Track modifier state for filtering terminal writes
    current_modifiers: Modifiers,
    // Help modal
    show_help: bool,
    // Workspace settings modal
    workspace_settings_open: bool,
    workspace_settings_idx: usize,
    workspace_settings_new_key: String,
    workspace_settings_new_value: String,
    // Tab picker popup (Option+click on "+")
    tab_picker_visible: bool,
    // Configured agent presets
    agent_presets: Vec<AgentPreset>,
    // Quick commands (app-level, run in bottom terminal)
    quick_commands: Vec<QuickCommand>,
    // Quick commands picker visibility
    quick_commands_visible: bool,
    // Track whether the window has focus (skip terminal processing when unfocused)
    window_focused: bool,
    // Track whether the bottom panel terminal has focus (vs main tab terminal)
    bottom_panel_focused: bool,
    /// Configs for workspaces that were closed but whose settings (env, color, etc.)
    /// should be preserved in workspaces.json for reopening later.
    closed_workspace_configs: Vec<WorkspaceConfig>,
    workspaces_dirty: bool,
    next_workspace_save_at: Option<Instant>,
    log_server_dirty: bool,
    next_log_server_sync_at: Instant,
    next_perf_report_at: Instant,
    log_server_sync_in_flight: bool,
    log_server_sync_queued: bool,
    last_log_server_snapshot_hash: Option<u64>,
    // Speech-to-text state
    #[cfg(feature = "stt")]
    stt_enabled: bool,
    #[cfg(feature = "stt")]
    stt_recording: bool,
    #[cfg(feature = "stt")]
    stt_context: Option<Arc<whisper_rs::WhisperContext>>,
    #[cfg(feature = "stt")]
    stt_audio_buffer: Arc<Mutex<Vec<f32>>>,
    #[cfg(feature = "stt")]
    stt_stream: Option<cpal::Stream>,
    #[cfg(feature = "stt")]
    stt_sample_rate: u32,
    #[cfg(feature = "stt")]
    stt_transcribing: bool,
    /// What kind of content the singleton webview is currently hosting. Used
    /// to decide whether a tab activation can keep / update the webview in
    /// place or has to destroy + recreate it (the IPC handler is set at
    /// construction and can't be swapped, so Static<->Agent transitions need
    /// a recreate).
    webview_kind: WebviewKind,
    /// The agent tab id whose state the webview is currently mirroring (if
    /// `webview_kind == Agent`). Used to detect tab-switch-to-different-agent
    /// transitions which need a buffer replay.
    webview_agent_tab_id: Option<usize>,
}

/// What kind of content the singleton wry webview is currently hosting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebviewKind {
    None,
    /// Markdown / Excalidraw / HTML file viewer — no IPC handler.
    Static,
    /// Agent chat — installed once with an IPC handler at construction.
    Agent,
    /// Plans viewer — URL-loaded surface served by warp on localhost.
    PlansViewer,
}

const SPINE_WIDTH: f32 = 16.0;

const SLIDE_DURATION_MS: f32 = 400.0;
const SWIPE_DEBOUNCE_MS: u64 = 150;
const EDGE_PEEK_ZONE: f32 = 30.0;

fn workspace_scrollable_id() -> iced::widget::Id {
    iced::widget::Id::new("ws-slide")
}

fn tab_scrollable_id() -> iced::widget::Id {
    iced::widget::Id::new("tab-bar-scroll")
}

fn workspace_bar_scrollable_id() -> iced::widget::Id {
    iced::widget::Id::new("ws-bar-scroll")
}

fn file_view_scrollable_id() -> iced::widget::Id {
    iced::widget::Id::new("file-view-scroll")
}

const ESTIMATED_TAB_WIDTH: f32 = 200.0;
const ESTIMATED_WS_BTN_WIDTH: f32 = 180.0;

const MIN_FONT_SIZE: f32 = 10.0;
const MAX_FONT_SIZE: f32 = 24.0;
const FONT_SIZE_STEP: f32 = 1.0;
const WORKSPACES_SAVE_DEBOUNCE_MS: u64 = 1500;
const LOG_SERVER_SYNC_INTERVAL_MS: u64 = 15000;
const LOG_SERVER_STARTUP_RETRY_MS: u64 = 500;
const MENU_POLL_INTERVAL_MS: u64 = 200;
const GIT_POLL_FAST_INTERVAL_MS: u64 = 5000;
const GIT_POLL_MEDIUM_INTERVAL_MS: u64 = 10000;
const GIT_POLL_SLOW_INTERVAL_MS: u64 = 15000;
const GIT_POLL_IDLE_INTERVAL_MS: u64 = 30000;
const GIT_POLL_NON_REPO_INTERVAL_MS: u64 = 20000;

#[derive(Debug, Clone)]
pub struct GitStatusSnapshot {
    tab_id: usize,
    repo_path: PathBuf,
    repo_name: String,
    branch_name: String,
    is_git_repo: bool,
    staged: Vec<FileEntry>,
    unstaged: Vec<FileEntry>,
    untracked: Vec<FileEntry>,
}

#[derive(Debug, Clone)]
pub struct FileTreeSnapshot {
    tab_id: usize,
    current_dir: PathBuf,
    entries: Vec<FileTreeEntry>,
}

#[derive(Debug, Clone)]
pub struct DiffSnapshot {
    tab_id: usize,
    file_path: String,
    is_staged: bool,
    lines: Vec<DiffLine>,
    diff_syntax_lines: Option<Vec<Vec<SyntaxHighlightSegment>>>,
    diff_syntax_notice: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FileLoadSnapshot {
    tab_id: usize,
    path: PathBuf,
    file_content: String,
    image_path: Option<PathBuf>,
    webview_content: Option<String>,
    file_preview_notice: Option<String>,
    syntax_highlight_lines: Option<Vec<SyntaxHighlightLine>>,
    syntax_highlight_notice: Option<String>,
    file_signature: Option<FileVersionSignature>,
}

#[derive(Debug, Clone)]
pub struct FileSyntaxSnapshot {
    tab_id: usize,
    path: PathBuf,
    syntax_highlight_lines: Option<Vec<SyntaxHighlightLine>>,
    syntax_highlight_notice: Option<String>,
    file_signature: Option<FileVersionSignature>,
}

impl App {
    /// UI font size
    fn ui_font(&self) -> f32 {
        self.ui_font_size
    }

    /// Small UI font size (for hints, secondary text)
    fn ui_font_small(&self) -> f32 {
        self.ui_font_size - 1.0
    }

    /// Ghost/pill button style — transparent bg, subtle border, rounded, hover tint.
    /// Used for toolbar action buttons (Close, Open in Browser, Copy All, etc.)
    fn ghost_button_style(&self) -> impl Fn(&Theme, button::Status) -> button::Style {
        let border_color = self.theme.overlay0();
        let hover_bg = self.theme.bg_overlay();
        let text_color = self.theme.text_primary();
        let text_muted = self.theme.overlay1();
        move |_theme, status| {
            let (bg, tc) = match status {
                button::Status::Hovered => (Some(hover_bg.into()), text_color),
                button::Status::Pressed => (Some(hover_bg.into()), text_color),
                _ => (None, text_muted),
            };
            button::Style {
                background: bg,
                text_color: tc,
                border: iced::Border {
                    color: border_color,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                ..Default::default()
            }
        }
    }

    fn tab_uses_inline_webview(tab: &TabState) -> bool {
        let Some(fv) = tab.file_viewer() else {
            return false;
        };
        let is_markdown_webview =
            TabState::is_markdown_file(&fv.path) && fv.webview_content.is_some();
        let is_html = TabState::is_html_file(&fv.path);

        #[cfg(feature = "excalidraw")]
        let is_excalidraw = excalidraw::is_excalidraw_file(&fv.path);
        #[cfg(not(feature = "excalidraw"))]
        let is_excalidraw = false;

        is_excalidraw || is_markdown_webview || is_html
    }

    fn active_inline_webview_html(&self) -> Option<String> {
        self.active_tab().and_then(|tab| {
            if Self::tab_uses_inline_webview(tab) {
                tab.file_viewer().and_then(|fv| fv.webview_content.clone())
            } else {
                None
            }
        })
    }

    fn maybe_report_perf(&mut self, now: Instant) {
        if !perf_enabled() || now < self.next_perf_report_at {
            return;
        }

        let mut tab_count = 0usize;
        let mut viewing_files = 0usize;
        let mut preview_notice_count = 0usize;
        let mut file_content_bytes = 0usize;
        let mut webview_html_bytes = 0usize;
        let mut console_line_count = 0usize;
        let mut console_bytes = 0usize;

        for ws in &self.workspaces {
            console_line_count += ws.console.output_lines.len();
            console_bytes += ws
                .console
                .output_lines
                .iter()
                .map(|l| l.timestamp.len() + 1 + l.content.len())
                .sum::<usize>();

            for tab in &ws.tabs {
                tab_count += 1;
                if let Some(fv) = tab.file_viewer() {
                    viewing_files += 1;
                    if fv.preview_notice.is_some() {
                        preview_notice_count += 1;
                    }
                    file_content_bytes += fv.file_content.len();
                    webview_html_bytes += fv.webview_content.as_ref().map(|s| s.len()).unwrap_or(0);
                }
            }
        }

        perf_log!(
            "mem tabs={} viewing_files={} file_bytes={}KB webview_bytes={}KB console_bytes={}KB console_lines={} notices={}",
            tab_count,
            viewing_files,
            file_content_bytes / 1024,
            webview_html_bytes / 1024,
            console_bytes / 1024,
            console_line_count,
            preview_notice_count
        );

        self.next_perf_report_at = now + Duration::from_millis(PERF_REPORT_INTERVAL_MS);
    }

    /// Focus the active main tab terminal (unfocusing bottom panel terminal)
    fn focus_main_terminal(&mut self) -> Task<Event> {
        self.bottom_panel_focused = false;
        if let Some(ws) = self.active_workspace() {
            if let Some(tab) = ws.active_tab() {
                if let Some(term) = tab.terminal() {
                    return TerminalView::focus(term.widget_id().clone());
                }
            }
        }
        Task::none()
    }

    /// Focus a bottom panel terminal (unfocusing main tab terminal)
    fn focus_bottom_terminal(&mut self, idx: usize) -> Task<Event> {
        self.bottom_panel_focused = true;
        if let Some(ws) = self.active_workspace() {
            if let Some(bt) = ws.bottom_terminals.get(idx) {
                if let Some(term) = &bt.terminal {
                    return TerminalView::focus(term.widget_id().clone());
                }
            }
        }
        Task::none()
    }

    fn scroll_to_active_tab(&self) -> Task<Event> {
        let active_tab = self.active_workspace().map(|ws| ws.active_tab).unwrap_or(0);
        let target_x = (active_tab as f32 * ESTIMATED_TAB_WIDTH).max(0.0);
        iced::advanced::widget::operate(iced::advanced::widget::operation::scrollable::scroll_to(
            tab_scrollable_id(),
            scrollable::AbsoluteOffset {
                x: Some(target_x),
                y: None,
            },
        ))
    }

    fn scroll_to_active_workspace_bar(&self) -> Task<Event> {
        let target_x = (self.active_workspace_idx as f32 * ESTIMATED_WS_BTN_WIDTH).max(0.0);
        iced::advanced::widget::operate(iced::advanced::widget::operation::scrollable::scroll_to(
            workspace_bar_scrollable_id(),
            scrollable::AbsoluteOffset {
                x: Some(target_x),
                y: None,
            },
        ))
    }

    fn save_config(&self) {
        let started = Instant::now();
        let config = Config {
            terminal_font_size: self.terminal_font_size,
            ui_font_size: self.ui_font_size,
            sidebar_width: self.sidebar_width,
            scrollback_lines: self.scrollback_lines,
            font_size: None,
            theme: match self.theme {
                AppTheme::Dark => "dark".to_string(),
                AppTheme::Light => "light".to_string(),
            },
            show_hidden: self.show_hidden,
            console_height: self.console_height,
            console_expanded: self.console_expanded,
            log_server_enabled: self.log_server_enabled,
            #[cfg(feature = "stt")]
            stt_enabled: self.stt_enabled,
            #[cfg(feature = "stt")]
            stt_model_path: None,
            agent_presets: self.agent_presets.clone(),
            quick_commands: self.quick_commands.clone(),
        };
        config.save();
        let elapsed = started.elapsed();
        if elapsed > Duration::from_millis(100) {
            freeze_debug!("save_config took {}ms", elapsed.as_millis());
        }
    }

    fn save_workspaces(&self) {
        // Active workspaces from runtime state
        let mut configs: Vec<WorkspaceConfig> = self
            .workspaces
            .iter()
            .map(|ws| WorkspaceConfig {
                name: ws.name.clone(),
                abbrev: ws.abbrev.clone(),
                dir: ws.dir.to_string_lossy().to_string(),
                color: ws.color,
                tabs: ws
                    .tabs
                    .iter()
                    .map(|tab| WorkspaceTabConfig {
                        dir: tab.current_dir.to_string_lossy().to_string(),
                        repo_dir: Some(tab.repo_path.to_string_lossy().to_string()),
                        startup_command: tab.startup_command().map(String::from),
                        tab_kind: match &tab.kind {
                            TabKind::Terminal(_) => None, // omitted for backward compat
                            TabKind::Agent(_) => Some("agent".to_string()),
                        },
                        agent_config: tab.agent_session().map(|s| s.config.clone()),
                    })
                    .collect(),
                run_command: ws.console.run_command.clone(),
                bottom_terminals: ws
                    .bottom_terminals
                    .iter()
                    .map(|bt| BottomTerminalConfig {
                        dir: bt.cwd.to_string_lossy().to_string(),
                    })
                    .collect(),
                env: ws.env.clone(),
                active: true,
            })
            .collect();

        // Append closed workspace configs so their env/settings survive
        // (skip any whose dir matches an active workspace — it was reopened)
        for closed in &self.closed_workspace_configs {
            let already_active = configs.iter().any(|c| c.dir == closed.dir);
            if !already_active {
                configs.push(closed.clone());
            }
        }

        let ws_file = WorkspacesFile {
            workspaces: configs,
            active_workspace: self.active_workspace_idx,
        };
        ws_file.save();
    }

    /// Load workspace profiles from ~/.config/gitterm/profiles.json
    /// Returns a map of profile_name -> env vars
    fn load_profiles() -> Option<HashMap<String, HashMap<String, String>>> {
        let path = config::global_config_dir().join("profiles.json");
        let contents = std::fs::read_to_string(&path).ok()?;
        #[derive(serde::Deserialize)]
        struct Profile {
            #[allow(dead_code)]
            description: Option<String>,
            env: HashMap<String, String>,
        }
        let profiles: HashMap<String, Profile> = serde_json::from_str(&contents).ok()?;
        Some(
            profiles
                .into_iter()
                .map(|(name, p)| (name, p.env))
                .collect(),
        )
    }

    fn mark_workspaces_dirty(&mut self) {
        self.workspaces_dirty = true;
        self.next_workspace_save_at =
            Some(Instant::now() + Duration::from_millis(WORKSPACES_SAVE_DEBOUNCE_MS));
    }

    fn handle_terminal_backend_command(
        term: &mut iced_term::Terminal,
        cmd: iced_term::backend::Command,
        context: &str,
        window_focused: bool,
    ) -> iced_term::actions::Action {
        // Stamp the watchdog with the specific backend-command kind. The
        // generic dispatcher heartbeat skips `Event::Terminal` (it fires at
        // PTY-tick rate), so without this tag a freeze inside `term.handle`
        // would surface as "Last event: CheckMenu-done" — useless. With this,
        // a watchdog dump names the exact sub-path (Backend:Write,
        // Backend:Resize, Backend:ProcessAlacrittyEvent, …) so we know
        // whether we're parked in PTY write, term-lock contention, etc.
        let cmd_name = match &cmd {
            iced_term::backend::Command::Write(_) => "Write",
            iced_term::backend::Command::Scroll(_) => "Scroll",
            iced_term::backend::Command::Resize(..) => "Resize",
            iced_term::backend::Command::SelectStart(..) => "SelectStart",
            iced_term::backend::Command::SelectUpdate(_) => "SelectUpdate",
            iced_term::backend::Command::ClearSelection => "ClearSelection",
            iced_term::backend::Command::ProcessLink(..) => "ProcessLink",
            iced_term::backend::Command::MouseReport(..) => "MouseReport",
            iced_term::backend::Command::ProcessAlacrittyEvent(_) => "ProcessAlacrittyEvent",
        };
        heartbeat(&format!("Backend:{}:{}", cmd_name, context));
        let started = Instant::now();
        let action = if window_focused {
            term.handle(iced_term::Command::ProxyToBackend(cmd))
        } else {
            // Skip expensive sync/redraw when window is not visible.
            // Avoids font cache cold-start stalls during sleep/wake cycles.
            term.handle_no_redraw(iced_term::Command::ProxyToBackend(cmd))
        };
        let elapsed = started.elapsed();
        if elapsed > Duration::from_millis(100) {
            freeze_debug!(
                "terminal.handle in {} took {}ms (cmd={})",
                context,
                elapsed.as_millis(),
                cmd_name,
            );
        }
        // Mark survival so the watchdog can distinguish "froze in handle"
        // from "froze somewhere downstream that consumed the Action".
        heartbeat(&format!("Backend:{}:{}-done", cmd_name, context));
        action
    }

    fn start_log_server(&self) {
        let server_state = self.log_server_state.clone();
        tokio::spawn(async move {
            log_server::start_server(server_state).await;
        });
    }

    /// Push the active workspace's markdown document directories into
    /// `ServerState` so the local viewer routes can resolve them.
    fn sync_plans_dir(&self) {
        let workspace_dir = self
            .workspaces
            .get(self.active_workspace_idx)
            .map(|ws| &ws.dir);
        if let Ok(mut p) = self.log_server_state.plans_dir.write() {
            *p = workspace_dir.map(|dir| dir.join(".plans"));
        }
        if let Ok(mut p) = self.log_server_state.docs_dir.write() {
            *p = workspace_dir.map(|dir| dir.join("docs"));
        }
    }

    fn set_log_server_enabled(&mut self, enabled: bool) {
        if self.log_server_enabled == enabled {
            return;
        }

        self.log_server_enabled = enabled;

        if enabled {
            self.last_log_server_snapshot_hash = None;
            self.log_server_dirty = true;
            self.log_server_sync_queued = false;
            self.next_log_server_sync_at = Instant::now();
            self.start_log_server();
        } else {
            self.log_server_state.shutdown.notify_one();
            if let Ok(mut port) = self.log_server_state.bound_port.lock() {
                *port = None;
            }
            self.log_server_dirty = false;
            self.log_server_sync_in_flight = false;
            self.log_server_sync_queued = false;
            self.last_log_server_snapshot_hash = None;
        }

        self.save_config();
    }

    fn mark_log_server_dirty(&mut self) {
        if self.log_server_enabled {
            self.log_server_dirty = true;
        }
    }

    fn queue_log_server_sync(&mut self) -> Task<Event> {
        if !self.log_server_enabled {
            self.log_server_dirty = false;
            self.log_server_sync_queued = false;
            self.next_log_server_sync_at =
                Instant::now() + Duration::from_millis(LOG_SERVER_SYNC_INTERVAL_MS);
            return Task::none();
        }

        if self.log_server_sync_in_flight {
            self.log_server_sync_queued = true;
            return Task::none();
        }

        // If the localhost log server is still starting, retry soon.
        if self.log_server_state.base_url().is_none() {
            self.log_server_dirty = true;
            self.next_log_server_sync_at =
                Instant::now() + Duration::from_millis(LOG_SERVER_STARTUP_RETRY_MS);
            return Task::none();
        }

        self.log_server_dirty = false;
        self.next_log_server_sync_at =
            Instant::now() + Duration::from_millis(LOG_SERVER_SYNC_INTERVAL_MS);

        // PERFORMANCE: Terminal content extraction (get_all_text) is expensive — it
        // locks each terminal mutex and iterates the entire scrollback history.
        // With many tabs this caused 80+ second freezes on the main thread.
        //
        // Mitigation: budget a max time for the collection phase and bail early
        // if it takes too long.  Each individual get_all_text() call is still
        // synchronous (Terminal is not Send), but we cap total collection time.

        let started = Instant::now();
        const LOG_SYNC_BUDGET_MS: u128 = 200; // Max 200ms on main thread

        let state = self.log_server_state.clone();
        let mut terminal_snapshots = std::collections::HashMap::new();
        let mut file_snapshots = std::collections::HashMap::new();
        let mut terminal_bytes = 0usize;
        let mut file_bytes = 0usize;
        let mut snapshot_hasher = DefaultHasher::new();
        let mut budget_exceeded = false;

        // Collect terminal content and file content from all tabs across all workspaces
        for tab in self.workspaces.iter().flat_map(|ws| ws.tabs.iter()) {
            tab.id.hash(&mut snapshot_hasher);

            if let Some(term) = tab.terminal() {
                // Check time budget before expensive get_all_text()
                if started.elapsed().as_millis() > LOG_SYNC_BUDGET_MS {
                    budget_exceeded = true;
                    false.hash(&mut snapshot_hasher);
                    continue;
                }
                true.hash(&mut snapshot_hasher);
                let content = term.get_all_text();
                terminal_bytes += content.len();
                tab.repo_name.hash(&mut snapshot_hasher);
                content.hash(&mut snapshot_hasher);
                let snapshot = log_server::TerminalSnapshot {
                    tab_id: tab.id,
                    tab_name: tab.repo_name.clone(),
                    content,
                };
                terminal_snapshots.insert(tab.id, snapshot);
            } else {
                false.hash(&mut snapshot_hasher);
            }

            // If tab is viewing a file, add it to file snapshots
            if let Some(fv) = tab.file_viewer() {
                if !fv.file_content.is_empty() {
                    true.hash(&mut snapshot_hasher);
                    file_bytes += fv.file_content.len();
                    fv.path.to_string_lossy().hash(&mut snapshot_hasher);
                    fv.file_content.hash(&mut snapshot_hasher);
                    let snapshot = log_server::FileSnapshot {
                        file_path: fv.path.to_string_lossy().to_string(),
                        content: fv.file_content.clone(),
                    };
                    file_snapshots.insert(tab.id, snapshot);
                } else {
                    false.hash(&mut snapshot_hasher);
                }
            } else {
                false.hash(&mut snapshot_hasher);
            }
        }

        if budget_exceeded {
            freeze_debug!(
                "log_sync budget exceeded ({}ms) - skipped some terminals",
                started.elapsed().as_millis()
            );
        }

        let snapshot_hash = snapshot_hasher.finish();
        if self.last_log_server_snapshot_hash == Some(snapshot_hash) {
            perf_log!(
                "log_sync skip unchanged terminals={} files={} collect_took={}ms",
                terminal_snapshots.len(),
                file_snapshots.len(),
                started.elapsed().as_millis()
            );
            return Task::none();
        }
        self.last_log_server_snapshot_hash = Some(snapshot_hash);
        self.log_server_sync_in_flight = true;

        perf_log!(
            "log_sync terminals={} files={} term_bytes={}KB file_bytes={}KB collect_took={}ms",
            terminal_snapshots.len(),
            file_snapshots.len(),
            terminal_bytes / 1024,
            file_bytes / 1024,
            started.elapsed().as_millis()
        );

        Task::perform(
            async move {
                let mut terminals = state.terminals.write().await;
                *terminals = terminal_snapshots;
                let mut files = state.files.write().await;
                *files = file_snapshots;
            },
            |_| Event::LogServerSyncComplete,
        )
    }

    fn request_git_status(tab_id: usize, repo_path: PathBuf) -> Task<Event> {
        let fallback_repo_path = repo_path.clone();
        Task::perform(
            async move {
                match tokio::task::spawn_blocking(move || collect_git_status(tab_id, repo_path))
                    .await
                {
                    Ok(snapshot) => snapshot,
                    Err(err) => {
                        freeze_debug!("CRITICAL: spawn_blocking failed! Skipping git status for tab {} ({}): {}",
                                     tab_id, fallback_repo_path.display(), err);
                        // Return empty snapshot instead of blocking main thread
                        GitStatusSnapshot {
                            tab_id,
                            repo_path: fallback_repo_path,
                            repo_name: "unknown".to_string(),
                            branch_name: "main".to_string(),
                            is_git_repo: true,
                            staged: Vec::new(),
                            unstaged: Vec::new(),
                            untracked: Vec::new(),
                        }
                    }
                }
            },
            Event::GitStatusLoaded,
        )
    }

    fn request_syntect_warmup() -> Task<Event> {
        Task::perform(
            async {
                let _ = tokio::task::spawn_blocking(warm_syntect_engine).await;
            },
            |_| Event::SyntectWarmupComplete,
        )
    }

    fn request_file_tree(tab_id: usize, current_dir: PathBuf, show_hidden: bool) -> Task<Event> {
        let fallback_dir = current_dir.clone();
        Task::perform(
            async move {
                match tokio::task::spawn_blocking(move || {
                    collect_file_tree(tab_id, current_dir, show_hidden)
                })
                .await
                {
                    Ok(snapshot) => snapshot,
                    Err(_) => FileTreeSnapshot {
                        tab_id,
                        current_dir: fallback_dir,
                        entries: Vec::new(),
                    },
                }
            },
            Event::FileTreeLoaded,
        )
    }

    fn request_diff(
        tab_id: usize,
        repo_path: PathBuf,
        file_path: String,
        staged: bool,
        is_dark_theme: bool,
    ) -> Task<Event> {
        let fallback_repo_path = repo_path.clone();
        let fallback_file_path = file_path.clone();
        Task::perform(
            async move {
                match tokio::task::spawn_blocking(move || {
                    let mut snapshot = collect_diff(tab_id, repo_path, file_path, staged);
                    let (syntax_lines, syntax_notice) = build_diff_syntax_highlight_lines_cached(
                        &snapshot.file_path,
                        snapshot.is_staged,
                        &snapshot.lines,
                        is_dark_theme,
                    );
                    snapshot.diff_syntax_lines = syntax_lines;
                    snapshot.diff_syntax_notice = syntax_notice;
                    snapshot
                })
                .await
                {
                    Ok(snapshot) => snapshot,
                    Err(_) => {
                        let mut snapshot =
                            collect_diff(tab_id, fallback_repo_path, fallback_file_path, staged);
                        let (syntax_lines, syntax_notice) =
                            build_diff_syntax_highlight_lines_cached(
                                &snapshot.file_path,
                                snapshot.is_staged,
                                &snapshot.lines,
                                is_dark_theme,
                            );
                        snapshot.diff_syntax_lines = syntax_lines;
                        snapshot.diff_syntax_notice = syntax_notice;
                        snapshot
                    }
                }
            },
            Event::DiffLoaded,
        )
    }

    fn request_file_load(tab_id: usize, path: PathBuf, is_dark_theme: bool) -> Task<Event> {
        let fallback_path = path.clone();
        Task::perform(
            async move {
                match tokio::task::spawn_blocking(move || {
                    collect_file_load(tab_id, path, is_dark_theme)
                })
                .await
                {
                    Ok(snapshot) => snapshot,
                    Err(_) => FileLoadSnapshot {
                        tab_id,
                        path: fallback_path,
                        file_content: String::new(),
                        image_path: None,
                        webview_content: None,
                        file_preview_notice: None,
                        syntax_highlight_lines: None,
                        syntax_highlight_notice: None,
                        file_signature: None,
                    },
                }
            },
            Event::FileLoaded,
        )
    }

    fn request_file_syntax_highlight(
        tab_id: usize,
        path: PathBuf,
        file_content: String,
        is_dark_theme: bool,
        file_signature: Option<FileVersionSignature>,
        max_lines: usize,
    ) -> Task<Event> {
        let fallback_path = path.clone();
        Task::perform(
            async move {
                match tokio::task::spawn_blocking(move || {
                    collect_file_syntax_highlight(
                        tab_id,
                        path,
                        file_content,
                        is_dark_theme,
                        file_signature,
                        max_lines,
                    )
                })
                .await
                {
                    Ok(snapshot) => snapshot,
                    Err(_) => FileSyntaxSnapshot {
                        tab_id,
                        path: fallback_path,
                        syntax_highlight_lines: None,
                        syntax_highlight_notice: None,
                        file_signature: None,
                    },
                }
            },
            Event::FileSyntaxHighlighted,
        )
    }
}

impl App {
    fn new() -> (Self, Task<Event>) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let config = Config::load();

        let theme = if config.theme == "light" {
            AppTheme::Light
        } else {
            AppTheme::Dark
        };

        // Handle migration from old single font_size config
        let (terminal_font, ui_font) = if let Some(old_size) = config.font_size {
            (old_size, old_size - 1.0)
        } else {
            (config.terminal_font_size, config.ui_font_size)
        };
        let log_server_enabled = config.log_server_enabled;

        // Initialize log server state
        let log_server_state = log_server::ServerState::new();

        let mut app = Self {
            title: String::from("GitTerm"),
            workspaces: Vec::new(),
            active_workspace_idx: 0,
            next_tab_id: 0,
            theme,
            terminal_font_size: terminal_font.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE),
            ui_font_size: ui_font.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE),
            sidebar_width: config.sidebar_width.clamp(150.0, 600.0),
            scrollback_lines: config.scrollback_lines,
            sidebar_collapsed: false,
            dragging_divider: false,
            show_hidden: config.show_hidden,
            window_size: (1400.0, 800.0), // Initial size, updated on resize
            log_server_state,
            log_server_enabled,
            console_expanded: config.console_expanded,
            console_height: config.console_height.clamp(32.0, 600.0),
            dragging_console_divider: false,
            editing_console_command: None,
            slide_offset: 0.0,
            slide_target: 0.0,
            slide_animating: false,
            slide_start_time: None,
            slide_start_offset: 0.0,
            last_user_scroll: None,
            edge_peek_left: false,
            edge_peek_right: false,
            attention_pulse_bright: false,
            current_modifiers: Modifiers::empty(),
            show_help: false,
            workspace_settings_open: false,
            workspace_settings_idx: 0,
            workspace_settings_new_key: String::new(),
            workspace_settings_new_value: String::new(),
            tab_picker_visible: false,
            agent_presets: config.agent_presets.clone(),
            quick_commands: config.quick_commands.clone(),
            quick_commands_visible: false,
            window_focused: true,
            bottom_panel_focused: false,
            closed_workspace_configs: Vec::new(),
            workspaces_dirty: false,
            next_workspace_save_at: None,
            log_server_dirty: log_server_enabled,
            next_log_server_sync_at: Instant::now(),
            next_perf_report_at: Instant::now() + Duration::from_millis(PERF_REPORT_INTERVAL_MS),
            log_server_sync_in_flight: false,
            log_server_sync_queued: false,
            last_log_server_snapshot_hash: None,
            // Speech-to-text
            #[cfg(feature = "stt")]
            stt_enabled: config.stt_enabled,
            #[cfg(feature = "stt")]
            stt_recording: false,
            #[cfg(feature = "stt")]
            stt_context: None,
            #[cfg(feature = "stt")]
            stt_audio_buffer: Arc::new(Mutex::new(Vec::new())),
            #[cfg(feature = "stt")]
            stt_stream: None,
            #[cfg(feature = "stt")]
            stt_sample_rate: 48000,
            #[cfg(feature = "stt")]
            stt_transcribing: false,
            webview_kind: WebviewKind::None,
            webview_agent_tab_id: None,
        };

        // Try to restore workspaces from saved config
        if let Some(ws_file) = WorkspacesFile::load() {
            for ws_config in &ws_file.workspaces {
                // Keep closed workspaces in memory so their env/color/settings
                // are preserved in workspaces.json and restored on reopen.
                if !ws_config.active {
                    app.closed_workspace_configs.push(ws_config.clone());
                    continue;
                }
                let dir = PathBuf::from(&ws_config.dir);
                let home = std::env::var("HOME").unwrap_or_default();
                // If workspace dir is $HOME, name the workspace after its first tab's repo instead
                let name = if dir == Path::new(&home) {
                    ws_config
                        .tabs
                        .first()
                        .map(|t| PathBuf::from(&t.dir))
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                        .unwrap_or_else(|| ws_config.name.clone())
                } else {
                    ws_config.name.clone()
                };
                let mut workspace = Workspace::new(name, dir.clone(), ws_config.color);
                workspace.abbrev = ws_config.abbrev.clone();
                workspace.env = ws_config.env.clone();
                // Restore saved run command if present
                if let Some(cmd) = &ws_config.run_command {
                    workspace.console.run_command = Some(cmd.clone());
                    workspace.console.status = ConsoleStatus::Stopped;
                }

                if ws_config.tabs.is_empty() {
                    // Always have at least one tab
                    app.add_tab_to_workspace(&mut workspace, dir);
                } else {
                    for tab_config in &ws_config.tabs {
                        let tab_dir = PathBuf::from(&tab_config.dir);
                        let repo_dir = tab_config
                            .repo_dir
                            .as_ref()
                            .map(PathBuf::from)
                            .or_else(|| {
                                Repository::discover(&tab_dir)
                                    .ok()
                                    .and_then(|repo| repo.workdir().map(PathBuf::from))
                            })
                            .unwrap_or_else(|| tab_dir.clone());
                        let current_dir = if tab_dir.is_dir() {
                            tab_dir
                        } else {
                            repo_dir.clone()
                        };
                        match (
                            tab_config.tab_kind.as_deref(),
                            tab_config.agent_config.as_ref(),
                        ) {
                            (Some("agent"), Some(agent_config)) => {
                                app.add_agent_tab_to_workspace(
                                    &mut workspace,
                                    repo_dir,
                                    Some(current_dir),
                                    agent_config.clone(),
                                );
                            }
                            (Some("agent"), None) => {
                                eprintln!(
                                    "WARN: tab_kind=\"agent\" without agent_config; falling back to terminal tab for {}",
                                    repo_dir.display()
                                );
                                app.add_tab_to_workspace_with_command(
                                    &mut workspace,
                                    repo_dir,
                                    Some(current_dir),
                                    tab_config.startup_command.clone(),
                                );
                            }
                            (Some(other), _) if other != "terminal" => {
                                eprintln!(
                                    "WARN: unknown tab_kind={:?}; falling back to terminal tab for {}",
                                    other,
                                    repo_dir.display()
                                );
                                app.add_tab_to_workspace_with_command(
                                    &mut workspace,
                                    repo_dir,
                                    Some(current_dir),
                                    tab_config.startup_command.clone(),
                                );
                            }
                            _ => {
                                app.add_tab_to_workspace_with_command(
                                    &mut workspace,
                                    repo_dir,
                                    Some(current_dir),
                                    tab_config.startup_command.clone(),
                                );
                            }
                        }
                    }
                }

                // Restore bottom panel terminals
                for bt_config in &ws_config.bottom_terminals {
                    let bt = app.create_bottom_terminal(PathBuf::from(&bt_config.dir));
                    workspace.bottom_terminals.push(bt);
                }

                app.workspaces.push(workspace);
            }
            app.active_workspace_idx = ws_file
                .active_workspace
                .min(app.workspaces.len().saturating_sub(1));
        }

        // If no workspaces were loaded, create one from the current directory
        if app.workspaces.is_empty() {
            let dir = cwd;
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "Home".to_string());
            let mut workspace = Workspace::new(name, dir.clone(), WorkspaceColor::Lavender);
            app.add_tab_to_workspace(&mut workspace, dir);
            app.workspaces.push(workspace);
        }

        if app.log_server_enabled {
            app.start_log_server();
        }
        app.sync_plans_dir();

        // Set initial slide position for active workspace
        let viewport_width = app.content_viewport_width();
        let initial_offset = app.active_workspace_idx as f32 * viewport_width;
        app.slide_offset = initial_offset;
        app.slide_target = initial_offset;

        // Return startup tasks (menu init + initial git status for active tab)
        let mut startup_tasks = vec![Task::done(Event::InitMenu), Self::request_syntect_warmup()];
        if let Some((tab_id, repo_path)) = {
            if let Some(tab) = app.active_tab_mut() {
                tab.git_status_loading = true;
                tab.last_poll = Instant::now();
                Some((tab.id, tab.repo_path.clone()))
            } else {
                None
            }
        } {
            startup_tasks.push(Self::request_git_status(tab_id, repo_path));
        }

        // If the active tab on startup is an agent tab (restored from
        // workspaces.json), dispatch its activation so the webview gets
        // built — TabSelect doesn't fire automatically on restore.
        let active_agent_tab_id = app
            .active_tab()
            .filter(|t| matches!(t.kind, TabKind::Agent(_)))
            .map(|t| t.id);
        if let Some(tab_id) = active_agent_tab_id {
            startup_tasks.push(app.show_agent_webview(tab_id));
        }

        (app, Task::batch(startup_tasks))
    }

    fn add_tab_to_workspace(&mut self, workspace: &mut Workspace, repo_path: PathBuf) {
        let tab = self.create_tab_for_workspace(repo_path, None, Some(&workspace.name.clone()));
        workspace.tabs.push(tab);
        workspace.active_tab = workspace.tabs.len() - 1;
    }

    fn add_tab_to_workspace_with_command(
        &mut self,
        workspace: &mut Workspace,
        repo_path: PathBuf,
        current_dir: Option<PathBuf>,
        startup_command: Option<String>,
    ) {
        let mut tab = self.create_tab_for_workspace(
            repo_path.clone(),
            startup_command,
            Some(&workspace.name.clone()),
        );
        if let Some(dir) = current_dir {
            tab.current_dir = dir;
        } else {
            tab.current_dir = repo_path;
        }
        workspace.tabs.push(tab);
        workspace.active_tab = workspace.tabs.len() - 1;
    }

    /// Construct an agent tab (no terminal, no shell). The subprocess for the
    /// configured backend is not started here — Step 3 (TRU-29) will spawn it
    /// when the user submits the first prompt.
    fn add_agent_tab_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        repo_path: PathBuf,
        current_dir: Option<PathBuf>,
        config: AgentBackendConfig,
    ) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let mut tab = TabState::new(id, repo_path.clone());
        tab.kind = TabKind::Agent(AgentSession::new(config));
        if let Some(dir) = current_dir {
            tab.current_dir = dir;
        } else {
            tab.current_dir = repo_path;
        }
        workspace.tabs.push(tab);
        workspace.active_tab = workspace.tabs.len() - 1;
    }

    fn add_tab(&mut self, repo_path: PathBuf) {
        let tab = self.create_tab(repo_path, None);
        if let Some(ws) = self.active_workspace_mut() {
            ws.tabs.push(tab);
            ws.active_tab = ws.tabs.len() - 1;
        }
    }

    fn add_tab_with_command(&mut self, repo_path: PathBuf, startup_command: Option<String>) {
        let tab = self.create_tab(repo_path, startup_command);
        if let Some(ws) = self.active_workspace_mut() {
            ws.tabs.push(tab);
            ws.active_tab = ws.tabs.len() - 1;
        }
    }

    /// Build terminal settings for a given working directory and optional startup command.
    /// Extracted so create_tab, create_bottom_terminal, and recreate_terminals can share this logic.
    fn build_terminal_settings(
        cwd: &std::path::Path,
        startup_command: Option<&str>,
        scrollback_lines: usize,
        theme: &AppTheme,
        terminal_font_size: f32,
        extra_env: &[(&str, &str)],
    ) -> iced_term::settings::Settings {
        #[cfg(target_os = "windows")]
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string());

        #[cfg(not(target_os = "windows"))]
        let shell = std::env::var("SHELL")
            .ok()
            .or_else(|| {
                let user = std::env::var("USER").ok()?;
                let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
                for line in passwd.lines() {
                    let parts: Vec<&str> = line.split(':').collect();
                    if parts.first() == Some(&user.as_str()) {
                        return parts.get(6).map(|s| s.to_string());
                    }
                }
                None
            })
            .unwrap_or_else(|| "/bin/zsh".to_string());

        let mut env = std::collections::HashMap::new();

        #[cfg(not(target_os = "windows"))]
        {
            env.insert("TERM".to_string(), "xterm-256color".to_string());
            env.insert("COLORTERM".to_string(), "truecolor".to_string());
            env.insert(
                "LANG".to_string(),
                std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".to_string()),
            );
            if let Ok(home) = std::env::var("HOME") {
                env.insert("HOME".to_string(), home.clone());
                env.insert("PATH".to_string(), format!("{}/.local/bin:{}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin", home, home));
            }
            if let Ok(user) = std::env::var("USER") {
                env.insert("USER".to_string(), user.clone());
                env.insert("LOGNAME".to_string(), user);
            }
            env.insert("SHELL".to_string(), shell.clone());
        }

        #[cfg(target_os = "windows")]
        {
            for (key, value) in std::env::vars() {
                env.insert(key, value);
            }
        }

        env.insert("GITTERM_PRECMD".to_string(), "1".to_string());

        if let Some(cmd) = startup_command {
            env.insert("GITTERM_STARTUP_CMD".to_string(), cmd.to_string());
        }

        for (key, value) in extra_env {
            env.insert(key.to_string(), value.to_string());
        }

        env.insert("CLAUDECODE".to_string(), String::new());
        env.insert("CLAUDE_CODE_ENTRYPOINT".to_string(), String::new());

        let is_zsh = shell.contains("zsh");
        let is_bash = shell.contains("bash");
        let is_windows = cfg!(target_os = "windows");

        let args = if is_windows {
            if let Some(cmd) = startup_command {
                let lower = shell.to_lowercase();
                if lower.contains("powershell") || lower.contains("pwsh") {
                    vec![
                        "-NoExit".to_string(),
                        "-Command".to_string(),
                        cmd.to_string(),
                    ]
                } else {
                    vec!["/K".to_string(), cmd.to_string()]
                }
            } else {
                vec![]
            }
        } else if is_zsh {
            let home = std::env::var("HOME").unwrap_or_default();
            let gitterm_dir = format!("{home}/.config/gitterm/zsh");
            let gitterm_zshrc = format!("{gitterm_dir}/.zshrc");

            let _ = std::fs::create_dir_all(&gitterm_dir);
            let zshrc_content = format!(
                r#"# GitTerm shell integration - sets terminal title on directory change
_gitterm_set_title() {{ print -Pn "\e]0;%~\a" }}
autoload -Uz add-zsh-hook
add-zsh-hook precmd _gitterm_set_title
add-zsh-hook chpwd _gitterm_set_title
_gitterm_set_title
[[ -f "{home}/.zshenv" ]] && source "{home}/.zshenv"
[[ -f "{home}/.zprofile" ]] && source "{home}/.zprofile"
[[ -f "{home}/.zshrc" ]] && source "{home}/.zshrc"
if [[ -n "$GITTERM_STARTUP_CMD" ]]; then
    _gitterm_cmd="$GITTERM_STARTUP_CMD"
    unset GITTERM_STARTUP_CMD
    eval "$_gitterm_cmd"
    unset _gitterm_cmd
fi
"#
            );
            let _ = std::fs::write(&gitterm_zshrc, zshrc_content);

            env.insert("ZDOTDIR".to_string(), gitterm_dir);
            vec!["-l".to_string()]
        } else if is_bash {
            let prompt_cmd = r#"printf "\e]0;%s\a" "$PWD"; if [[ -n "$GITTERM_STARTUP_CMD" ]]; then _c="$GITTERM_STARTUP_CMD"; unset GITTERM_STARTUP_CMD; eval "$_c"; unset _c; fi"#;
            env.insert("PROMPT_COMMAND".to_string(), prompt_cmd.to_string());
            vec!["-l".to_string()]
        } else {
            vec!["-l".to_string()]
        };

        iced_term::settings::Settings {
            backend: iced_term::settings::BackendSettings {
                program: shell,
                args,
                working_directory: Some(cwd.to_path_buf()),
                scrollback_lines,
                env,
            },
            theme: iced_term::settings::ThemeSettings::new(Box::new(theme.terminal_palette())),
            font: iced_term::settings::FontSettings {
                size: terminal_font_size,
                ..Default::default()
            },
        }
    }

    /// Standard noop bindings for keys we handle as app shortcuts.
    fn standard_noop_bindings() -> Vec<(
        iced_term::bindings::KeyboardBinding,
        iced_term::bindings::BindingAction,
    )> {
        use iced_term::bindings::{BindingAction, InputKind, KeyboardBinding as Binding};
        let mut bindings = vec![(
            Binding {
                target: InputKind::Char("`".to_string()),
                modifiers: Modifiers::CTRL,
                terminal_mode_include: iced_term::TermMode::empty(),
                terminal_mode_exclude: iced_term::TermMode::empty(),
            },
            BindingAction::Noop,
        )];
        for n in 1..=9u8 {
            bindings.push((
                Binding {
                    target: InputKind::Char(n.to_string()),
                    modifiers: Modifiers::CTRL,
                    terminal_mode_include: iced_term::TermMode::empty(),
                    terminal_mode_exclude: iced_term::TermMode::empty(),
                },
                BindingAction::Noop,
            ));
        }
        bindings
    }

    fn create_tab(&mut self, repo_path: PathBuf, startup_command: Option<String>) -> TabState {
        self.create_tab_for_workspace(repo_path, startup_command, None)
    }

    fn create_tab_for_workspace(
        &mut self,
        repo_path: PathBuf,
        startup_command: Option<String>,
        workspace_name: Option<&str>,
    ) -> TabState {
        // Read env fresh from the global workspaces.json. Caller passes the workspace name
        // explicitly so this works correctly during init (before active_workspace is set).
        let ws_name = workspace_name
            .map(|s| s.to_string())
            .or_else(|| self.active_workspace().map(|ws| ws.name.clone()));
        let global_ws_path = config::global_config_dir().join("workspaces.json");
        let extra_env: Vec<(String, String)> = ws_name
            .and_then(|name| {
                std::fs::read_to_string(&global_ws_path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<WorkspacesFile>(&s).ok())
                    .and_then(|f| f.workspaces.into_iter().find(|w| w.name == name))
            })
            .map(|cfg| cfg.env.into_iter().collect())
            .unwrap_or_default();
        let extra_env_refs: Vec<(&str, &str)> = extra_env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let id = self.next_tab_id;
        self.next_tab_id += 1;

        let mut tab = TabState::new(id, repo_path.clone());
        tab.set_startup_command(startup_command.clone());

        let settings = Self::build_terminal_settings(
            &repo_path,
            startup_command.as_deref(),
            self.scrollback_lines,
            &self.theme,
            self.terminal_font_size,
            &extra_env_refs,
        );

        if let Ok(mut terminal) = iced_term::Terminal::new(id as u64, settings) {
            terminal.handle(iced_term::Command::AddBindings(
                Self::standard_noop_bindings(),
            ));
            tab.set_terminal(terminal);
        }

        tab
    }

    fn create_bottom_terminal(&mut self, cwd: PathBuf) -> BottomTerminal {
        let ws_name = self.active_workspace().map(|ws| ws.name.clone());
        let global_ws_path = config::global_config_dir().join("workspaces.json");
        let extra_env: Vec<(String, String)> = ws_name
            .and_then(|name| {
                std::fs::read_to_string(&global_ws_path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<WorkspacesFile>(&s).ok())
                    .and_then(|f| f.workspaces.into_iter().find(|w| w.name == name))
            })
            .map(|cfg| cfg.env.into_iter().collect())
            .unwrap_or_default();
        let extra_env_refs: Vec<(&str, &str)> = extra_env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let settings = Self::build_terminal_settings(
            &cwd,
            None,
            self.scrollback_lines,
            &self.theme,
            self.terminal_font_size,
            &extra_env_refs,
        );
        let terminal = iced_term::Terminal::new(id as u64, settings)
            .ok()
            .map(|mut t| {
                t.handle(iced_term::Command::AddBindings(
                    Self::standard_noop_bindings(),
                ));
                t
            });
        BottomTerminal {
            id,
            terminal,
            title: None,
            cwd,
        }
    }

    /// Width of the content area (window width minus spine)
    fn content_viewport_width(&self) -> f32 {
        (self.window_size.0 - SPINE_WIDTH).max(1.0)
    }

    fn active_workspace(&self) -> Option<&Workspace> {
        self.workspaces.get(self.active_workspace_idx)
    }

    fn active_workspace_mut(&mut self) -> Option<&mut Workspace> {
        self.workspaces.get_mut(self.active_workspace_idx)
    }

    fn active_tab(&self) -> Option<&TabState> {
        self.active_workspace().and_then(|ws| ws.active_tab())
    }

    fn active_tab_mut(&mut self) -> Option<&mut TabState> {
        self.active_workspace_mut()
            .and_then(|ws| ws.active_tab_mut())
    }

    fn any_tab_needs_attention(&self) -> bool {
        self.workspaces.iter().any(|ws| ws.has_attention())
    }

    fn title(&self) -> String {
        if let Some(ws) = self.active_workspace() {
            if let Some(tab) = ws.active_tab() {
                format!("{} - {} - {}", self.title, ws.name, tab.repo_name)
            } else {
                format!("{} - {}", self.title, ws.name)
            }
        } else {
            self.title.clone()
        }
    }

    fn subscription(&self) -> Subscription<Event> {
        let mut subs = vec![
            // Agent webview IPC bridge — drains the global mpsc into events.
            // Built once via a fn pointer; re-runs are guarded by the
            // identity hash of `Subscription::run`'s function-pointer arg.
            Subscription::run(agent_ipc_stream),
            iced::time::every(Duration::from_millis(5000)).map(|_| Event::Tick),
            // Poll menu events frequently
            iced::time::every(Duration::from_millis(MENU_POLL_INTERVAL_MS))
                .map(|_| Event::CheckMenu),
            iced::event::listen_with(|event, _status, _id| match event {
                iced::Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                    Some(Event::ModifiersChanged(modifiers))
                }
                iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                    Some(Event::KeyPressed(key, modifiers))
                }
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                    Some(Event::MouseMoved(position.x, position.y))
                }
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                )) => Some(Event::DividerDragEnd),
                iced::Event::Window(iced::window::Event::Resized(size)) => {
                    Some(Event::WindowResized(size.width, size.height))
                }
                iced::Event::Window(iced::window::Event::CloseRequested) => {
                    Some(Event::WindowCloseRequested)
                }
                iced::Event::Window(iced::window::Event::Focused) => Some(Event::WindowFocused),
                iced::Event::Window(iced::window::Event::Unfocused) => Some(Event::WindowUnfocused),
                _ => None,
            }),
        ];

        // Animation tick (~60fps) — when animating or waiting for swipe debounce
        if self.slide_animating || self.last_user_scroll.is_some() {
            subs.push(
                iced::time::every(Duration::from_millis(16)).map(|_| Event::SlideAnimationTick),
            );
        }

        // Attention pulse (500ms toggle) — when any tab needs attention or STT recording
        #[cfg(feature = "stt")]
        let stt_recording = self.stt_recording;
        #[cfg(not(feature = "stt"))]
        let stt_recording = false;
        if self.any_tab_needs_attention() || stt_recording {
            subs.push(
                iced::time::every(Duration::from_millis(500)).map(|_| Event::AttentionPulseTick),
            );
        }

        // Keep UI responsive for delayed loading indicators.
        let loading_in_progress = self
            .workspaces
            .iter()
            .flat_map(|ws| ws.tabs.iter())
            .any(|tab| {
                tab.file_viewer()
                    .map(|fv| fv.load_in_progress)
                    .unwrap_or(false)
                    || tab.diff_load_in_progress
            });
        if loading_in_progress {
            subs.push(iced::time::every(Duration::from_millis(50)).map(|_| Event::LoadingUiTick));
        }

        for ws in &self.workspaces {
            for tab in &ws.tabs {
                if let Some(term) = tab.terminal() {
                    subs.push(
                        term.subscription()
                            .with(tab.id)
                            .map(|(tab_id, e)| Event::Terminal(tab_id, e)),
                    );
                }
            }
            // Bottom panel terminal subscriptions
            for bt in &ws.bottom_terminals {
                if let Some(term) = &bt.terminal {
                    subs.push(
                        term.subscription()
                            .with(bt.id)
                            .map(|(id, e)| Event::BottomTerminalEvent(id, e)),
                    );
                }
            }
        }

        Subscription::batch(subs)
    }

    fn update(&mut self, event: Event) -> Task<Event> {
        // Heartbeat for freeze watchdog — skip high-frequency terminal events
        match &event {
            Event::Terminal(_, _) | Event::BottomTerminalEvent(_, _) => {}
            Event::CheckMenu => {
                heartbeat("CheckMenu");
            }
            Event::SlideAnimationTick => {
                heartbeat("SlideAnimationTick");
            }
            Event::AttentionPulseTick => {
                heartbeat("AttentionPulseTick");
            }
            Event::LoadingUiTick => {
                heartbeat("LoadingUiTick");
            }
            other => {
                heartbeat(&format!("{:?}", other).chars().take(80).collect::<String>());
            }
        }
        match event {
            Event::MainTerminalClicked => {
                if self.bottom_panel_focused {
                    return self.focus_main_terminal();
                }
            }
            Event::BottomTerminalClicked(idx) => {
                if !self.bottom_panel_focused {
                    return self.focus_bottom_terminal(idx);
                }
            }
            Event::Terminal(tab_id, iced_term::Event::BackendCall(_, cmd)) => {
                let focused = self.window_focused;
                // Main terminal received input — it has focus
                if matches!(&cmd, iced_term::backend::Command::Write(_)) {
                    self.bottom_panel_focused = false;
                }
                // Don't forward keyboard input to terminal while editing console command or console search
                if self.editing_console_command.is_some() {
                    return Task::none();
                }
                if self.console_expanded {
                    if let Some(ws) = self.active_workspace() {
                        if ws.console.search_visible {
                            if let iced_term::backend::Command::Write(_) = &cmd {
                                return Task::none();
                            }
                        }
                    }
                }
                // Suppress terminal writes for keys we handle as app shortcuts (Ctrl+1-9, Ctrl+`)
                if self.current_modifiers.control() && !self.current_modifiers.command() {
                    if let iced_term::backend::Command::Write(ref data) = cmd {
                        if data.len() == 1 {
                            let b = data[0];
                            if (b'1'..=b'9').contains(&b) || b == b'`' {
                                return Task::none();
                            }
                        }
                    }
                }
                // Suppress Alt+/ (help modal shortcut) — macOS sends ÷ (0xC3 0xB7)
                if self.current_modifiers.alt() {
                    if let iced_term::backend::Command::Write(ref data) = cmd {
                        if data == &[0xC3, 0xB7] || data == b"/" {
                            return Task::none();
                        }
                    }
                }
                let mut pending_task: Option<Task<Event>> = None;
                if let Some(tab) = self
                    .workspaces
                    .iter_mut()
                    .flat_map(|ws| ws.tabs.iter_mut())
                    .find(|t| t.id == tab_id)
                {
                    // Clear attention on user keyboard input (Write), not on process output (ProcessAlacrittyEvent)
                    if matches!(&cmd, iced_term::backend::Command::Write(_)) {
                        if tab.needs_attention {
                            tab.needs_attention = false;
                        }
                        // Reset git poll to fast when user is actively typing
                        if tab.git_poll_interval_ms > GIT_POLL_FAST_INTERVAL_MS {
                            tab.git_poll_interval_ms = GIT_POLL_FAST_INTERVAL_MS;
                            tab.git_unchanged_streak = 0;
                        }
                    }
                    if let Some(term) = tab.terminal_mut() {
                        match Self::handle_terminal_backend_command(
                            term,
                            cmd,
                            "main_terminal_event",
                            focused,
                        ) {
                            iced_term::actions::Action::Shutdown => {}
                            iced_term::actions::Action::ChangeTitle(title) => {
                                // Set tab-specific title
                                tab.set_terminal_title(Some(title.clone()));
                                // Detect attention: Claude Code sets "✳" (U+2733) prefix when waiting for input
                                tab.needs_attention = title.starts_with('✳');

                                // Try to sync sidebar directory from terminal title
                                if let Some(dir) = TabState::extract_dir_from_title(&title) {
                                    if dir != tab.current_dir {
                                        tab.current_dir = dir.clone();
                                        pending_task = Some(Self::request_file_tree(
                                            tab.id,
                                            dir.clone(),
                                            self.show_hidden,
                                        ));

                                        // Trigger a git status refresh — the worker will
                                        // discover the correct repo root off the main thread.
                                        tab.repo_path = dir.clone();
                                        tab.git_poll_interval_ms = GIT_POLL_FAST_INTERVAL_MS;
                                        tab.git_unchanged_streak = 0;
                                        tab.last_git_status_hash = None;
                                        tab.last_poll = Instant::now();
                                        tab.git_status_loading = true;
                                        let status_task =
                                            Self::request_git_status(tab.id, tab.repo_path.clone());
                                        pending_task =
                                            Some(if let Some(tree_task) = pending_task.take() {
                                                Task::batch([tree_task, status_task])
                                            } else {
                                                status_task
                                            });
                                    }
                                }
                            }
                            iced_term::actions::Action::ClipboardStore(text) => {
                                // OSC 52: program requested clipboard write
                                pending_task = Some(iced::clipboard::write(text));
                            }
                            iced_term::actions::Action::ClipboardLoad(formatter) => {
                                // OSC 52: program requested clipboard read.
                                // Read clipboard, format the response, write back to PTY.
                                let tid = tab_id;
                                pending_task = Some(iced::clipboard::read().map(move |content| {
                                    let text = content.unwrap_or_default();
                                    let response = formatter(&text);
                                    Event::Terminal(
                                        tid,
                                        iced_term::Event::BackendCall(
                                            tid as u64,
                                            iced_term::backend::Command::Write(
                                                response.into_bytes(),
                                            ),
                                        ),
                                    )
                                }));
                            }
                            _ => {}
                        }
                    }
                }
                self.mark_log_server_dirty();
                if let Some(task) = pending_task {
                    return task;
                }
            }
            Event::Tick => {
                let mut tasks: Vec<Task<Event>> = Vec::new();
                let workspace_dirty = false;

                // Poll git status for the active tab with adaptive cadence.
                if let Some(tab) = self.active_tab_mut() {
                    // NOTE: repo root self-heal moved to GitStatusLoaded handler
                    // to avoid blocking main thread with Repository::discover().

                    let git_focus_active =
                        tab.sidebar_mode == SidebarMode::Git || tab.selected_file.is_some();
                    let effective_git_poll_interval_ms = if git_focus_active {
                        tab.git_poll_interval_ms
                    } else {
                        tab.git_poll_interval_ms.max(GIT_POLL_SLOW_INTERVAL_MS)
                    };

                    if !tab.git_status_loading
                        && tab.last_poll.elapsed()
                            >= Duration::from_millis(effective_git_poll_interval_ms)
                    {
                        let tab_id = tab.id;
                        let repo_path = tab.repo_path.clone();
                        tab.last_poll = Instant::now();
                        tab.git_status_loading = true;
                        tasks.push(Self::request_git_status(tab_id, repo_path));
                    }
                }
                if workspace_dirty {
                    self.mark_workspaces_dirty();
                }

                // Debounced workspace persistence
                let now = Instant::now();
                self.maybe_report_perf(now);
                if self.workspaces_dirty
                    && self
                        .next_workspace_save_at
                        .is_some_and(|deadline| now >= deadline)
                {
                    self.save_workspaces();
                    self.workspaces_dirty = false;
                    self.next_workspace_save_at = None;
                }

                // Throttled/queued log server sync
                if self.log_server_dirty && now >= self.next_log_server_sync_at {
                    tasks.push(self.queue_log_server_sync());
                }

                if !tasks.is_empty() {
                    return Task::batch(tasks);
                }
            }
            Event::InitMenu => {
                // Initialize native macOS menu bar (must happen after NSApp exists)
                setup_menu_bar();
            }
            Event::CheckMenu => {
                let _check_menu_start = std::time::Instant::now();

                // Detect system wake: if wall clock jumped far ahead of monotonic expectations
                {
                    static LAST_CHECK: std::sync::Mutex<Option<std::time::Instant>> =
                        std::sync::Mutex::new(None);
                    if let Ok(mut last) = LAST_CHECK.lock() {
                        if let Some(prev) = *last {
                            let gap = prev.elapsed();
                            if gap > Duration::from_secs(10) {
                                freeze_debug!("WAKE DETECTED: {}s gap between CheckMenu events (system likely slept)", gap.as_secs());
                            }
                        }
                        *last = Some(std::time::Instant::now());
                    }
                }

                // Poll for native menu events
                if let Ok(event) = MenuEvent::receiver().try_recv() {
                    freeze_debug!("native menu event received: {:?}", event.id);
                    if let Some(ids) = MENU_IDS.get() {
                        if event.id == ids.increase_terminal_font {
                            freeze_debug!("dispatching native menu: increase_terminal_font");
                            return self.update(Event::IncreaseTerminalFont);
                        } else if event.id == ids.decrease_terminal_font {
                            freeze_debug!("dispatching native menu: decrease_terminal_font");
                            return self.update(Event::DecreaseTerminalFont);
                        } else if event.id == ids.increase_ui_font {
                            freeze_debug!("dispatching native menu: increase_ui_font");
                            return self.update(Event::IncreaseUiFont);
                        } else if event.id == ids.decrease_ui_font {
                            freeze_debug!("dispatching native menu: decrease_ui_font");
                            return self.update(Event::DecreaseUiFont);
                        } else if event.id == ids.toggle_theme {
                            freeze_debug!("dispatching native menu: toggle_theme");
                            return self.update(Event::ToggleTheme);
                        } else if event.id == ids.toggle_log_server {
                            freeze_debug!("dispatching native menu: toggle_log_server");
                            return self.update(Event::ToggleLogServer);
                        } else if event.id == ids.clear_terminal {
                            freeze_debug!("dispatching native menu: clear_terminal");
                            return self.update(Event::ClearTerminal);
                        } else if event.id == ids.open_plans_viewer {
                            freeze_debug!("dispatching native menu: open_plans_viewer");
                            return self.update(Event::OpenPlansViewer);
                        }
                    }
                }

                let _menu_poll_elapsed = _check_menu_start.elapsed();

                // Drain console output for all workspaces
                let _drain_start = std::time::Instant::now();
                let mut auto_expand = false;
                let mut console_changed = false;
                for ws in &mut self.workspaces {
                    // Take rx out to avoid double-borrow
                    if let Some(mut rx) = ws.console.output_rx.take() {
                        let mut exited_info = None;
                        let mut messages = Vec::new();
                        while let Ok(msg) = rx.try_recv() {
                            messages.push(msg);
                            if messages.len() >= 50 {
                                break;
                            }
                        }
                        for msg in messages {
                            match msg {
                                ConsoleOutputMessage::Stdout(line) => {
                                    ws.console.push_line(line, false);
                                    console_changed = true;
                                }
                                ConsoleOutputMessage::Stderr(line) => {
                                    ws.console.push_line(line, true);
                                    console_changed = true;
                                }
                                ConsoleOutputMessage::Exited(code) => {
                                    exited_info = Some(code);
                                    console_changed = true;
                                }
                            }
                        }
                        // Rebuild editor content once for the entire batch
                        ws.console.rebuild_if_dirty();
                        if let Some(code) = exited_info {
                            ws.console.exit_code = code;
                            ws.console.stopped_at = Some(std::time::Instant::now());
                            if code.is_some() && code != Some(0) {
                                ws.console.status = ConsoleStatus::Error;
                                auto_expand = true;
                            } else {
                                ws.console.status = ConsoleStatus::Stopped;
                            }
                            ws.console.child_killer = None;
                            // Don't put rx back — process is done
                        } else {
                            // Put rx back
                            ws.console.output_rx = Some(rx);
                        }
                    }
                }
                if auto_expand {
                    self.console_expanded = true;
                }
                let _drain_elapsed = _drain_start.elapsed();
                if console_changed {
                    self.mark_log_server_dirty();
                }
                let _check_menu_elapsed = _check_menu_start.elapsed();
                if _check_menu_elapsed > Duration::from_millis(50) {
                    freeze_debug!(
                        "CheckMenu handler took {}ms (menu_poll={}ms, console_drain={}ms)",
                        _check_menu_elapsed.as_millis(),
                        _menu_poll_elapsed.as_millis(),
                        _drain_elapsed.as_millis()
                    );
                }
                heartbeat("CheckMenu-done");
            }
            Event::TabSelect(idx) => {
                if let Some(ws) = self.active_workspace_mut() {
                    if idx < ws.tabs.len() {
                        ws.active_tab = idx;
                    }
                }
                let scroll_task = self.scroll_to_active_tab();
                // Agent tab takes priority over the static-webview path because
                // its webview is the tab's primary surface, not a modal overlay.
                let active_agent_tab_id = self
                    .active_tab()
                    .filter(|t| matches!(t.kind, TabKind::Agent(_)))
                    .map(|t| t.id);
                if let Some(tab_id) = active_agent_tab_id {
                    return Task::batch([scroll_task, self.show_agent_webview(tab_id)]);
                }
                if let Some(html) = self.active_inline_webview_html() {
                    let bounds = self.calculate_webview_bounds();
                    return Task::batch([scroll_task, self.show_webview(html, bounds)]);
                }
                // No webview kind is active for this tab. Hide whatever is
                // there but don't change webview_kind — the static webview
                // can be reused for the next markdown/file viewer.
                self.hide_webview_for_non_agent();
                return scroll_task;
            }
            Event::TabClose(idx) => {
                // Hide WebView when closing tabs
                self.hide_webview_for_non_agent();
                if let Some(ws) = self.active_workspace_mut() {
                    if idx < ws.tabs.len() && ws.tabs.len() > 1 {
                        ws.tabs.remove(idx);
                        if ws.active_tab >= ws.tabs.len() {
                            ws.active_tab = ws.tabs.len() - 1;
                        }
                    }
                }
                self.mark_workspaces_dirty();
                self.mark_log_server_dirty();
                return self.scroll_to_active_tab();
            }
            Event::AgentActivityLoaded(tab_id, result) => {
                // Find the tab by id and apply the loaded activity
                'outer_activity: for ws in &mut self.workspaces {
                    for tab in &mut ws.tabs {
                        if tab.id == tab_id {
                            tab.agent_sidebar.loading = false;
                            match result {
                                Ok(activity) => tab.agent_sidebar.activity = Some(activity),
                                Err(e) => {
                                    eprintln!("Failed to load agent activity: {}", e);
                                    tab.agent_sidebar.activity = Some(agent::AgentActivity::new());
                                }
                            }
                            break 'outer_activity;
                        }
                    }
                }
                return Task::none();
            }
            Event::AgentConversationLoaded(tab_id, conversation) => {
                'outer_conv: for ws in &mut self.workspaces {
                    for tab in &mut ws.tabs {
                        if tab.id == tab_id {
                            tab.agent_sidebar.conversation = Some(conversation);
                            break 'outer_conv;
                        }
                    }
                }
                return Task::none();
            }
            Event::SelectAgentCapture(idx) => {
                if let Some(tab) = self.active_tab_mut() {
                    if tab.agent_sidebar.selected_capture_idx == Some(idx) {
                        // Deselect if clicking the same one
                        tab.agent_sidebar.selected_capture_idx = None;
                        tab.agent_sidebar.conversation = None;
                    } else {
                        tab.agent_sidebar.selected_capture_idx = Some(idx);
                        tab.agent_sidebar.conversation = None; // Clear while loading
                                                               // Load conversation async
                        if let Some(activity) = &tab.agent_sidebar.activity {
                            if let Some(capture) = activity.captures.get(idx) {
                                let capture = capture.clone();
                                let tab_id = tab.id;
                                return Task::perform(
                                    async move {
                                        match tokio::task::spawn_blocking(move || {
                                            agent::Conversation::load_for_capture(&capture)
                                        })
                                        .await
                                        {
                                            Ok(conv) => (tab_id, conv),
                                            Err(_) => (
                                                tab_id,
                                                agent::Conversation {
                                                    entries: Vec::new(),
                                                    error: Some(
                                                        "Failed to load conversation".to_string(),
                                                    ),
                                                },
                                            ),
                                        }
                                    },
                                    |(tab_id, conv)| Event::AgentConversationLoaded(tab_id, conv),
                                );
                            }
                        }
                    }
                }
                return Task::none();
            }
            Event::LaunchAgentPreset(idx) => {
                // Option+click on "+" shows tab picker (but not if picker is already open)
                if self.current_modifiers.alt() && !self.tab_picker_visible {
                    self.tab_picker_visible = true;
                } else {
                    self.tab_picker_visible = false;
                    let command = self.agent_presets.get(idx).map(|p| p.command.clone());
                    if let Some(ws) = self.active_workspace() {
                        let dir = ws
                            .active_tab()
                            .map(|t| t.current_dir.clone())
                            .unwrap_or_else(|| ws.dir.clone());
                        self.add_tab_with_command(dir, command);
                        self.mark_workspaces_dirty();
                        self.mark_log_server_dirty();
                        if let Some((tab_id, repo_path)) = {
                            if let Some(tab) = self.active_tab_mut() {
                                tab.git_status_loading = true;
                                Some((tab.id, tab.repo_path.clone()))
                            } else {
                                None
                            }
                        } {
                            return Task::batch([
                                self.scroll_to_active_tab(),
                                Self::request_git_status(tab_id, repo_path),
                            ]);
                        }
                        return self.scroll_to_active_tab();
                    }
                }
            }
            Event::ResumeAgentPreset(idx) => {
                self.tab_picker_visible = false;
                let command = self
                    .agent_presets
                    .get(idx)
                    .and_then(|p| p.resume_command.clone().or_else(|| Some(p.command.clone())));
                if let Some(ws) = self.active_workspace() {
                    let dir = ws
                        .active_tab()
                        .map(|t| t.current_dir.clone())
                        .unwrap_or_else(|| ws.dir.clone());
                    self.add_tab_with_command(dir, command);
                    self.mark_workspaces_dirty();
                    self.mark_log_server_dirty();
                    if let Some((tab_id, repo_path)) = {
                        if let Some(tab) = self.active_tab_mut() {
                            tab.git_status_loading = true;
                            Some((tab.id, tab.repo_path.clone()))
                        } else {
                            None
                        }
                    } {
                        return Task::batch([
                            self.scroll_to_active_tab(),
                            Self::request_git_status(tab_id, repo_path),
                        ]);
                    }
                    return self.scroll_to_active_tab();
                }
            }
            Event::AgentWebviewIpc(msg) => {
                // The IPC bridge already tagged each message with a tab_id
                // (from window.__currentTabId in the JS). Translate into the
                // existing AgentSubmitPrompt / AgentStopRequested handlers.
                return match msg {
                    AgentIpcMessage::Submit { tab_id, text } => {
                        Task::done(Event::AgentSubmitPrompt(tab_id, text))
                    }
                    AgentIpcMessage::Stop { tab_id } => {
                        Task::done(Event::AgentStopRequested(tab_id))
                    }
                };
            }
            Event::AgentSubmitPrompt(tab_id, prompt) => {
                // Find the tab. If it's an Agent tab, ensure a subprocess
                // manager is spawned (lazy on first prompt) and forward the
                // prompt. The first spawn returns a Task that bridges the
                // event channel into AgentEventReceived events for this tab.
                let mut bridge: Option<tokio::sync::mpsc::UnboundedReceiver<tab::AgentEvent>> =
                    None;
                // Synthetic event so the user sees their own prompt rendered
                // immediately (the agent stream takes a few hundred ms before
                // the first system event arrives).
                let echo = tab::AgentEvent::Other(serde_json::json!({
                    "type": "user_prompt",
                    "text": prompt,
                }));
                'outer_submit: for ws in &mut self.workspaces {
                    for t in &mut ws.tabs {
                        if t.id != tab_id {
                            continue;
                        }
                        let repo_path = t.repo_path.clone();
                        let Some(session) = t.agent_session_mut() else {
                            eprintln!("AgentSubmitPrompt: tab {} is not an agent tab", tab_id);
                            return Task::none();
                        };
                        if session.task_handle.is_none() {
                            let handle = tab::spawn_agent_task(session.config.clone(), repo_path);
                            // Take the receiver up-front so this turn can wire
                            // it into a Task::run; it lives for the tab's lifetime.
                            bridge = handle.take_event_receiver();
                            session.task_handle = Some(handle);
                        }
                        if let Some(handle) = session.task_handle.as_ref() {
                            if let Err(e) = handle.submit_prompt(prompt.clone()) {
                                eprintln!("AgentSubmitPrompt failed: {}", e);
                            } else {
                                session.state = tab::AgentSessionState::Streaming;
                            }
                        }
                        session.conversation.push(echo.clone());
                        break 'outer_submit;
                    }
                }
                // Also push the echo into the webview if this tab is the one
                // currently rendered there.
                if self.webview_kind == WebviewKind::Agent
                    && self.webview_agent_tab_id == Some(tab_id)
                {
                    push_agent_event_to_webview(&echo);
                }
                if let Some(rx) = bridge {
                    use tokio_stream::wrappers::UnboundedReceiverStream;
                    let stream = UnboundedReceiverStream::new(rx);
                    return Task::run(stream, move |ev| Event::AgentEventReceived(tab_id, ev));
                }
                return Task::none();
            }
            Event::AgentStopRequested(tab_id) => {
                for ws in &mut self.workspaces {
                    for t in &mut ws.tabs {
                        if t.id == tab_id {
                            if let Some(session) = t.agent_session_mut() {
                                if let Some(handle) = session.task_handle.as_ref() {
                                    handle.request_stop();
                                }
                                session.state = tab::AgentSessionState::Stopped;
                            }
                            return Task::none();
                        }
                    }
                }
                return Task::none();
            }
            Event::AgentEventReceived(tab_id, ev) => {
                // Append to the conversation buffer; if this tab's webview is
                // currently visible, also push the event live for rendering.
                let is_active_in_webview = self.webview_kind == WebviewKind::Agent
                    && self.webview_agent_tab_id == Some(tab_id);
                for ws in &mut self.workspaces {
                    for t in &mut ws.tabs {
                        if t.id == tab_id {
                            if let Some(session) = t.agent_session_mut() {
                                // Heuristic state transition until typed parser
                                // lands (Step 9): `done`/`stopped` sentinels
                                // flip back to Idle/Stopped.
                                if let tab::AgentEvent::Other(value) = &ev {
                                    if let Some(t) = value.get("type").and_then(|v| v.as_str()) {
                                        if t == "done" {
                                            session.state = tab::AgentSessionState::Idle;
                                        } else if t == "stopped" {
                                            session.state = tab::AgentSessionState::Stopped;
                                        }
                                    }
                                }
                                if is_active_in_webview {
                                    push_agent_event_to_webview(&ev);
                                }
                                session.conversation.push(ev);
                            }
                            return Task::none();
                        }
                    }
                }
                return Task::none();
            }
            Event::DebugSpawnAgentTab => {
                // Step 3-only: spawn a pi-backed agent tab in the active workspace
                // and submit a hardcoded prompt so we can verify the subprocess
                // pipeline end-to-end. Removed before Step 4 lands.
                let model = std::env::var("PI_MODEL")
                    .unwrap_or_else(|_| "openai-codex/gpt-5.4".to_string());
                let session_path = format!("/tmp/gitterm-agent-debug-{}.jsonl", std::process::id());
                let config = tab::AgentBackendConfig::Pi {
                    model,
                    session_path: Some(session_path),
                    thinking: Some(true),
                };
                let (repo_path, current_dir) = match self.active_workspace() {
                    Some(ws) => {
                        let cd = ws
                            .active_tab()
                            .map(|t| t.current_dir.clone())
                            .unwrap_or_else(|| ws.dir.clone());
                        (ws.dir.clone(), Some(cd))
                    }
                    None => return Task::none(),
                };

                let id = self.next_tab_id;
                self.next_tab_id += 1;
                if let Some(ws) = self.active_workspace_mut() {
                    let mut tab = TabState::new(id, repo_path.clone());
                    tab.kind = TabKind::Agent(AgentSession::new(config));
                    tab.current_dir = current_dir.unwrap_or(repo_path);
                    ws.tabs.push(tab);
                    ws.active_tab = ws.tabs.len() - 1;
                }
                eprintln!("[debug] spawned agent tab id={}", id);
                // Surface the agent webview for the brand-new tab. The user
                // can type prompts directly into it; no hardcoded prompt now.
                let scroll_task = self.scroll_to_active_tab();
                return Task::batch([scroll_task, self.show_agent_webview(id)]);
            }
            Event::ShowQuickCommands => {
                if !self.quick_commands.is_empty() {
                    self.quick_commands_visible = true;
                }
            }
            Event::HideQuickCommands => {
                self.quick_commands_visible = false;
            }
            Event::RunQuickCommand(idx) => {
                self.quick_commands_visible = false;
                if let Some(qc) = self.quick_commands.get(idx).cloned() {
                    let cmd_bytes = format!("{}\n", qc.command).into_bytes();
                    // Find active bottom terminal, or create one
                    let has_bottom_terminal = self
                        .active_workspace()
                        .map(|ws| {
                            matches!(ws.active_bottom_tab, BottomPanelTab::Terminal(_))
                                && !ws.bottom_terminals.is_empty()
                        })
                        .unwrap_or(false);

                    if !has_bottom_terminal {
                        // Create a bottom terminal first
                        let dir = self
                            .active_workspace()
                            .map(|ws| {
                                ws.active_tab()
                                    .map(|t| t.current_dir.clone())
                                    .unwrap_or_else(|| ws.dir.clone())
                            })
                            .unwrap_or_else(|| PathBuf::from("."));
                        let bt = self.create_bottom_terminal(dir);
                        if let Some(ws) = self.active_workspace_mut() {
                            ws.bottom_terminals.push(bt);
                            let idx = ws.bottom_terminals.len() - 1;
                            ws.active_bottom_tab = BottomPanelTab::Terminal(idx);
                        }
                        self.console_expanded = true;
                    }

                    // Write the command to the active bottom terminal
                    if let Some(ws) = self.active_workspace_mut() {
                        if let BottomPanelTab::Terminal(bt_idx) = ws.active_bottom_tab {
                            if let Some(bt) = ws.bottom_terminals.get_mut(bt_idx) {
                                if let Some(term) = &mut bt.terminal {
                                    Self::handle_terminal_backend_command(
                                        term,
                                        iced_term::backend::Command::Write(cmd_bytes),
                                        "run_quick_command_bottom_terminal",
                                        true, // user-initiated, always redraw
                                    );
                                }
                            }
                        }
                    }
                    self.mark_workspaces_dirty();
                }
            }
            Event::NewPlainTab => {
                // Create a plain terminal tab (no startup command)
                self.tab_picker_visible = false;
                if let Some(ws) = self.active_workspace() {
                    let dir = ws
                        .active_tab()
                        .map(|t| t.current_dir.clone())
                        .unwrap_or_else(|| ws.dir.clone());
                    self.add_tab_with_command(dir, None);
                    self.mark_workspaces_dirty();
                    self.mark_log_server_dirty();
                    if let Some((tab_id, repo_path)) = {
                        if let Some(tab) = self.active_tab_mut() {
                            tab.git_status_loading = true;
                            Some((tab.id, tab.repo_path.clone()))
                        } else {
                            None
                        }
                    } {
                        return Task::batch([
                            self.scroll_to_active_tab(),
                            Self::request_git_status(tab_id, repo_path),
                        ]);
                    }
                    return self.scroll_to_active_tab();
                }
            }
            Event::ShowTabPicker => {
                self.tab_picker_visible = true;
            }
            Event::HideTabPicker => {
                self.tab_picker_visible = false;
            }
            Event::EditFile(path) => {
                // Open a file in $EDITOR (fallback: vim) in a new tab
                let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
                let cmd = format!("{} \"{}\"", editor, path.display());
                if let Some(ws) = self.active_workspace() {
                    let dir = ws
                        .active_tab()
                        .map(|t| t.current_dir.clone())
                        .unwrap_or_else(|| ws.dir.clone());
                    self.add_tab_with_command(dir, Some(cmd));
                    self.mark_workspaces_dirty();
                    self.mark_log_server_dirty();
                    if let Some((tab_id, repo_path)) = {
                        if let Some(tab) = self.active_tab_mut() {
                            tab.git_status_loading = true;
                            Some((tab.id, tab.repo_path.clone()))
                        } else {
                            None
                        }
                    } {
                        return Task::batch([
                            self.scroll_to_active_tab(),
                            Self::request_git_status(tab_id, repo_path),
                        ]);
                    }
                    return self.scroll_to_active_tab();
                }
            }
            Event::BottomTabSelect(tab) => {
                if let Some(ws) = self.active_workspace_mut() {
                    ws.active_bottom_tab = tab;
                }
                // Focus the appropriate terminal
                return match tab {
                    BottomPanelTab::Terminal(idx) => self.focus_bottom_terminal(idx),
                    BottomPanelTab::Console => self.focus_main_terminal(),
                };
            }
            Event::BottomTerminalAdd => {
                let dir = self
                    .active_workspace()
                    .map(|ws| {
                        ws.active_tab()
                            .map(|t| t.current_dir.clone())
                            .unwrap_or_else(|| ws.dir.clone())
                    })
                    .unwrap_or_else(|| PathBuf::from("."));
                let bt = self.create_bottom_terminal(dir);
                let bt_idx = if let Some(ws) = self.active_workspace_mut() {
                    ws.bottom_terminals.push(bt);
                    let idx = ws.bottom_terminals.len() - 1;
                    ws.active_bottom_tab = BottomPanelTab::Terminal(idx);
                    Some(idx)
                } else {
                    None
                };
                if let Some(idx) = bt_idx {
                    self.console_expanded = true;
                    self.mark_workspaces_dirty();
                    self.save_config();
                    return self.focus_bottom_terminal(idx);
                }
            }
            Event::BottomTerminalClose(idx) => {
                let was_active_terminal = self.active_workspace()
                    .map(|ws| matches!(ws.active_bottom_tab, BottomPanelTab::Terminal(i) if i == idx))
                    .unwrap_or(false);
                if let Some(ws) = self.active_workspace_mut() {
                    if idx < ws.bottom_terminals.len() {
                        ws.bottom_terminals.remove(idx);
                        // Fix active tab reference
                        match ws.active_bottom_tab {
                            BottomPanelTab::Terminal(active_idx) if active_idx == idx => {
                                ws.active_bottom_tab = BottomPanelTab::Console;
                            }
                            BottomPanelTab::Terminal(active_idx) if active_idx > idx => {
                                ws.active_bottom_tab = BottomPanelTab::Terminal(active_idx - 1);
                            }
                            _ => {}
                        }
                    }
                }
                self.mark_workspaces_dirty();
                // If we closed the active bottom terminal, refocus main terminal
                if was_active_terminal && self.bottom_panel_focused {
                    return self.focus_main_terminal();
                }
            }
            Event::BottomTerminalEvent(id, iced_term::Event::BackendCall(_, cmd)) => {
                let focused = self.window_focused;
                // Bottom terminal received input — it has focus
                if matches!(&cmd, iced_term::backend::Command::Write(_)) {
                    self.bottom_panel_focused = true;
                }
                // Suppress terminal writes for keys we handle as app shortcuts
                if self.current_modifiers.control() && !self.current_modifiers.command() {
                    if let iced_term::backend::Command::Write(ref data) = cmd {
                        if data.len() == 1 {
                            let b = data[0];
                            if (b'1'..=b'9').contains(&b) || b == b'`' {
                                return Task::none();
                            }
                        }
                    }
                }
                // Suppress Alt+/ (help modal shortcut)
                if self.current_modifiers.alt() {
                    if let iced_term::backend::Command::Write(ref data) = cmd {
                        if data == &[0xC3, 0xB7] || data == b"/" {
                            return Task::none();
                        }
                    }
                }
                if let Some(bt) = self
                    .workspaces
                    .iter_mut()
                    .flat_map(|ws| ws.bottom_terminals.iter_mut())
                    .find(|bt| bt.id == id)
                {
                    if let Some(term) = &mut bt.terminal {
                        match Self::handle_terminal_backend_command(
                            term,
                            cmd,
                            "bottom_terminal_event",
                            focused,
                        ) {
                            iced_term::actions::Action::Shutdown => {}
                            iced_term::actions::Action::ChangeTitle(title) => {
                                bt.title = Some(title);
                            }
                            iced_term::actions::Action::ClipboardStore(text) => {
                                return iced::clipboard::write(text);
                            }
                            iced_term::actions::Action::ClipboardLoad(formatter) => {
                                let tid = id;
                                return iced::clipboard::read().map(move |content| {
                                    let text = content.unwrap_or_default();
                                    let response = formatter(&text);
                                    Event::BottomTerminalEvent(
                                        tid,
                                        iced_term::Event::BackendCall(
                                            tid as u64,
                                            iced_term::backend::Command::Write(
                                                response.into_bytes(),
                                            ),
                                        ),
                                    )
                                });
                            }
                            _ => {}
                        }
                    }
                }
            }
            Event::OpenFolder => {
                return Task::perform(
                    async {
                        let folder = rfd::AsyncFileDialog::new()
                            .set_title("Select Folder")
                            .pick_folder()
                            .await;
                        folder.map(|f| f.path().to_path_buf())
                    },
                    Event::FolderSelected,
                );
            }
            Event::FolderSelected(Some(path)) => {
                // Allow any folder, not just git repos
                self.add_tab(path);
                self.mark_workspaces_dirty();
                self.mark_log_server_dirty();
                if let Some((tab_id, repo_path)) = {
                    if let Some(tab) = self.active_tab_mut() {
                        tab.git_status_loading = true;
                        Some((tab.id, tab.repo_path.clone()))
                    } else {
                        None
                    }
                } {
                    return Task::batch([
                        self.scroll_to_active_tab(),
                        Self::request_git_status(tab_id, repo_path),
                    ]);
                }
                return self.scroll_to_active_tab();
            }
            Event::FolderSelected(None) => {}
            Event::FileSelect(path, is_staged) => {
                // Hide WebView when switching to git diff view
                self.hide_webview_for_non_agent();
                let is_dark_theme = self.theme == AppTheme::Dark;

                if let Some(tab) = self.active_tab_mut() {
                    if tab.selected_file.as_deref() == Some(path.as_str())
                        && tab.selected_is_staged == is_staged
                        && (tab.diff_load_in_progress || !tab.diff_lines.is_empty())
                    {
                        return Task::none();
                    }

                    // Clear file viewer if open
                    tab.close_file_viewer();
                    // Find the index of this file
                    let all_files = tab.all_files();
                    if let Some(idx) = all_files.iter().position(|f| f.path == path) {
                        tab.file_index = idx as i32;
                    }
                    tab.selected_file = Some(path.clone());
                    tab.selected_is_staged = is_staged;
                    tab.diff_load_in_progress = true;
                    tab.diff_load_started_at = Some(Instant::now());
                    tab.diff_syntax_lines = None;
                    tab.diff_syntax_notice = None;
                    let tab_id = tab.id;
                    let repo_path = tab.repo_path.clone();
                    self.mark_log_server_dirty();
                    return Self::request_diff(tab_id, repo_path, path, is_staged, is_dark_theme);
                }
            }
            Event::FileSelectByIndex(idx) => {
                // Hide WebView when switching to git diff view
                self.hide_webview_for_non_agent();
                let is_dark_theme = self.theme == AppTheme::Dark;

                if let Some(tab) = self.active_tab_mut() {
                    // Clear file viewer if open
                    tab.close_file_viewer();

                    let total = tab.total_changes() as i32;
                    if total == 0 {
                        return Task::none();
                    }

                    let new_idx = idx.clamp(0, total - 1);
                    tab.file_index = new_idx;

                    let all_files = tab.all_files();
                    if let Some(file) = all_files.get(new_idx as usize) {
                        let path = file.path.clone();
                        let is_staged = file.is_staged;
                        if tab.selected_file.as_deref() == Some(path.as_str())
                            && tab.selected_is_staged == is_staged
                            && (tab.diff_load_in_progress || !tab.diff_lines.is_empty())
                        {
                            return Task::none();
                        }
                        tab.selected_file = Some(path.clone());
                        tab.selected_is_staged = is_staged;
                        tab.diff_load_in_progress = true;
                        tab.diff_load_started_at = Some(Instant::now());
                        tab.diff_syntax_lines = None;
                        tab.diff_syntax_notice = None;
                        let tab_id = tab.id;
                        let repo_path = tab.repo_path.clone();
                        self.mark_log_server_dirty();
                        return Self::request_diff(
                            tab_id,
                            repo_path,
                            path,
                            is_staged,
                            is_dark_theme,
                        );
                    }
                }
            }
            Event::ClearSelection => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.selected_file = None;
                    tab.file_index = -1;
                    tab.diff_lines.clear();
                    tab.diff_load_in_progress = false;
                    tab.diff_load_started_at = None;
                    tab.diff_syntax_lines = None;
                    tab.diff_syntax_notice = None;
                }
            }
            Event::KeyPressed(key, modifiers) => {
                self.current_modifiers = modifiers;

                // Workspace settings: Escape closes
                if self.workspace_settings_open
                    && matches!(key.as_ref(), Key::Named(key::Named::Escape))
                {
                    self.workspace_settings_open = false;
                    return Task::none();
                }

                // Tab picker: Escape closes
                if self.tab_picker_visible && matches!(key.as_ref(), Key::Named(key::Named::Escape))
                {
                    self.tab_picker_visible = false;
                    return Task::none();
                }

                // Quick commands picker: Escape closes
                if self.quick_commands_visible
                    && matches!(key.as_ref(), Key::Named(key::Named::Escape))
                {
                    self.quick_commands_visible = false;
                    return Task::none();
                }

                // Plans viewer: Escape dismisses
                if self.webview_kind == WebviewKind::PlansViewer
                    && matches!(key.as_ref(), Key::Named(key::Named::Escape))
                {
                    return self.update(Event::ClosePlansViewer);
                }

                // Help modal: Escape or Cmd+/ closes, all other keys consumed while open
                if self.show_help {
                    match key.as_ref() {
                        Key::Named(key::Named::Escape) => {
                            self.show_help = false;
                            return Task::none();
                        }
                        Key::Character(c) if c == "/" && modifiers.alt() => {
                            self.show_help = false;
                            return Task::none();
                        }
                        _ => return Task::none(),
                    }
                }

                // Option+/ (Alt+/) toggles help modal
                if modifiers.alt() && !modifiers.command() {
                    if let Key::Character(c) = key.as_ref() {
                        if c == "/" || c == "÷" {
                            return Task::done(Event::ToggleHelp);
                        }
                    }
                }

                // Escape cancels console command editing
                if self.editing_console_command.is_some() {
                    if let Key::Named(key::Named::Escape) = key.as_ref() {
                        return Task::done(Event::ConsoleCommandCancel);
                    }
                }

                // Console shortcuts (Cmd+J, Cmd+Shift+R) - before search shortcuts
                if modifiers.command() {
                    if let Key::Character(c) = key.as_ref() {
                        // TRU-29 Step 3 debug entry: Cmd+Shift+G spawns an agent tab
                        // and submits a hardcoded prompt. Removed before Step 4 ships.
                        if (c == "g" || c == "G") && modifiers.shift() {
                            return Task::done(Event::DebugSpawnAgentTab);
                        }
                        // Cmd+B - Toggle sidebar
                        if c == "b" && !modifiers.shift() {
                            return Task::done(Event::ToggleSidebar);
                        }
                        // Cmd+J - Toggle console panel
                        if c == "j" && !modifiers.shift() {
                            return Task::done(Event::ConsoleToggle);
                        }
                        // Cmd+Shift+R - Restart console process
                        if (c == "r" || c == "R") && modifiers.shift() {
                            return Task::done(Event::ConsoleRestart);
                        }
                        // Cmd+Shift+W - Close current workspace
                        if (c == "w" || c == "W") && modifiers.shift() {
                            return Task::done(Event::WorkspaceClose(self.active_workspace_idx));
                        }
                    }
                }

                // Console search shortcuts (Cmd+F when console active, Escape to close)
                if self.console_expanded {
                    if let Some(ws) = self.active_workspace() {
                        if ws.active_bottom_tab == BottomPanelTab::Console {
                            if modifiers.command() {
                                if let Key::Character(c) = key.as_ref() {
                                    if c == "f" {
                                        return Task::done(Event::ConsoleSearchToggle);
                                    }
                                }
                            }
                            if ws.console.search_visible {
                                if let Key::Named(key::Named::Escape) = key.as_ref() {
                                    return Task::done(Event::ConsoleSearchClose);
                                }
                            }
                        }
                    }
                }

                // Handle search shortcuts first (Cmd+F, Cmd+G, Escape when search active)
                if let Some(tab) = self.active_tab() {
                    // Search shortcuts
                    if modifiers.command() {
                        if let Key::Character(c) = key.as_ref() {
                            // Cmd+F - Toggle search
                            if c == "f" {
                                return Task::done(Event::ToggleSearch);
                            }
                            // Cmd+G / Cmd+Shift+G - Next/Prev match
                            if c == "g" && tab.search.is_active {
                                if modifiers.shift() {
                                    return Task::done(Event::SearchPrev);
                                } else {
                                    return Task::done(Event::SearchNext);
                                }
                            }
                            // Cmd+K - Clear terminal
                            if c == "k" {
                                return Task::done(Event::ClearTerminal);
                            }
                        }
                    }

                    // Escape - Close search if active
                    if tab.search.is_active {
                        if let Key::Named(key::Named::Escape) = key.as_ref() {
                            return Task::done(Event::SearchClose);
                        }
                    }

                    // Handle Escape in file viewer
                    if tab.viewing_file_path().is_some() {
                        if let Key::Named(key::Named::Escape) = key.as_ref() {
                            return Task::done(Event::CloseFileView);
                        }
                    }

                    if let Some(selected) = &tab.selected_file {
                        // In diff view - handle navigation
                        match key.as_ref() {
                            Key::Named(key::Named::Escape) => {
                                return Task::done(Event::ClearSelection);
                            }
                            Key::Character("j") => {
                                let new_idx = tab.file_index + 1;
                                return Task::done(Event::FileSelectByIndex(new_idx));
                            }
                            Key::Character("k") => {
                                let new_idx = tab.file_index - 1;
                                return Task::done(Event::FileSelectByIndex(new_idx));
                            }
                            Key::Character("g") => {
                                return Task::done(Event::FileSelectByIndex(0));
                            }
                            Key::Character("G") => {
                                let last = (tab.total_changes() as i32) - 1;
                                return Task::done(Event::FileSelectByIndex(last));
                            }
                            Key::Character("e") => {
                                // Open selected file in $EDITOR
                                let full_path = tab.repo_path.join(selected);
                                return Task::done(Event::EditFile(full_path));
                            }
                            _ => {}
                        }
                    }
                }

                // Ctrl+Space — toggle speech-to-text recording
                #[cfg(feature = "stt")]
                if modifiers.control()
                    && !modifiers.command()
                    && !modifiers.shift()
                    && !modifiers.alt()
                {
                    if let Key::Named(key::Named::Space) = key.as_ref() {
                        return Task::done(Event::SttToggle);
                    }
                }

                // Ctrl+backtick — jump to next attention tab
                if modifiers.control() && !modifiers.command() {
                    if let Key::Character(c) = key.as_ref() {
                        if c == "`" {
                            return Task::done(Event::AttentionJumpNext);
                        }
                    }
                }

                // Option+Shift+T — new plain terminal tab (folder picker)
                // Option+Shift+1..N — launch agent preset by index
                if modifiers.alt() && modifiers.shift() {
                    if let Key::Character(c) = key.as_ref() {
                        if c == "t" || c == "T" {
                            return Task::done(Event::OpenFolder);
                        }
                        // Option+Shift+1 through 9 launches agent preset by index
                        if let Ok(num) = c.parse::<usize>() {
                            if num >= 1 && num <= self.agent_presets.len() {
                                return Task::done(Event::LaunchAgentPreset(num - 1));
                            }
                        }
                    }
                }

                // Workspace switching with Ctrl+1-9
                if modifiers.control() && !modifiers.command() {
                    if let Key::Character(c) = key.as_ref() {
                        if let Ok(num) = c.parse::<usize>() {
                            if (1..=9).contains(&num) && num <= self.workspaces.len() {
                                return Task::done(Event::WorkspaceSelect(num - 1));
                            }
                        }
                    }
                }

                // Tab switching with Cmd+1-9
                // Terminal font: Cmd+Plus/Minus, UI font: Cmd+Shift+Plus/Minus
                if modifiers.command() {
                    if let Key::Character(c) = key.as_ref() {
                        if c == "=" || c == "+" {
                            if modifiers.shift() {
                                return Task::done(Event::IncreaseUiFont);
                            } else {
                                return Task::done(Event::IncreaseTerminalFont);
                            }
                        } else if c == "-" || c == "_" {
                            if modifiers.shift() {
                                return Task::done(Event::DecreaseUiFont);
                            } else {
                                return Task::done(Event::DecreaseTerminalFont);
                            }
                        } else if let Ok(num) = c.parse::<usize>() {
                            let tab_count =
                                self.active_workspace().map(|ws| ws.tabs.len()).unwrap_or(0);
                            if (1..=9).contains(&num) && num <= tab_count {
                                return Task::done(Event::TabSelect(num - 1));
                            }
                        }
                    }
                }
            }
            Event::ToggleSidebar => {
                self.sidebar_collapsed = !self.sidebar_collapsed;
                // Update WebView bounds if active
                if webview::is_active() {
                    let bounds = self.calculate_webview_bounds();
                    webview::update_bounds(bounds.0, bounds.1, bounds.2, bounds.3);
                }
            }
            Event::SetSidebarMode(mode) => {
                // Expand sidebar if collapsed when switching modes
                if self.sidebar_collapsed {
                    self.sidebar_collapsed = false;
                }
                // Hide WebView when switching modes (but keep agent webview alive
                // — it's the agent tab's primary content, not a modal overlay).
                self.hide_webview_for_non_agent();

                if let Some(tab) = self.active_tab_mut() {
                    if tab.sidebar_mode != mode {
                        match mode {
                            SidebarMode::Git => {
                                // Switching to Git mode - clear file viewer and refresh status
                                tab.agent_sidebar.selected_capture_idx = None;
                                tab.agent_sidebar.conversation = None;
                                tab.close_file_viewer();
                                tab.diff_load_in_progress = false;
                                tab.diff_load_started_at = None;
                                tab.last_poll = Instant::now();
                                tab.git_status_loading = true;
                                let tab_id = tab.id;
                                let repo_path = tab.repo_path.clone();
                                tab.sidebar_mode = mode;
                                self.mark_log_server_dirty();
                                return Self::request_git_status(tab_id, repo_path);
                            }
                            SidebarMode::Files => {
                                // Switching to Files mode - clear git selection
                                tab.agent_sidebar.selected_capture_idx = None;
                                tab.agent_sidebar.conversation = None;
                                tab.selected_file = None;
                                tab.diff_lines.clear();
                                tab.diff_load_in_progress = false;
                                tab.diff_load_started_at = None;
                                tab.diff_syntax_lines = None;
                                tab.diff_syntax_notice = None;
                                let tab_id = tab.id;
                                let current_dir = tab.current_dir.clone();
                                tab.sidebar_mode = mode;
                                return Self::request_file_tree(
                                    tab_id,
                                    current_dir,
                                    self.show_hidden,
                                );
                            }
                            SidebarMode::Claude => {
                                // Switching to Claude mode - clear file viewer and git selection
                                tab.agent_sidebar.selected_capture_idx = None;
                                tab.agent_sidebar.conversation = None;
                                tab.close_file_viewer();
                                tab.selected_file = None;
                                tab.diff_lines.clear();
                                tab.diff_load_in_progress = false;
                                tab.diff_load_started_at = None;
                                tab.diff_syntax_lines = None;
                                tab.diff_syntax_notice = None;
                                tab.fetch_claude_config();
                            }
                            SidebarMode::Agent => {
                                // Switching to Agent mode - clear file viewer and git selection
                                tab.close_file_viewer();
                                tab.selected_file = None;
                                tab.diff_lines.clear();
                                tab.diff_load_in_progress = false;
                                tab.diff_load_started_at = None;
                                tab.diff_syntax_lines = None;
                                tab.diff_syntax_notice = None;
                                let task = tab.fetch_agent_activity();
                                tab.sidebar_mode = mode;
                                return task;
                            }
                            SidebarMode::Plans => {
                                // Switching to Plans mode - clear file viewer and git selection.
                                // Plan list is read fresh in view_plans_sidebar on each render.
                                tab.close_file_viewer();
                                tab.selected_file = None;
                                tab.diff_lines.clear();
                                tab.diff_load_in_progress = false;
                                tab.diff_load_started_at = None;
                                tab.diff_syntax_lines = None;
                                tab.diff_syntax_notice = None;
                            }
                        }
                        tab.sidebar_mode = mode;
                    }
                }
            }
            Event::ToggleClaudeSection(section) => {
                if let Some(tab) = self.active_tab_mut() {
                    if tab.claude_config.expanded.contains(&section) {
                        tab.claude_config.expanded.remove(&section);
                    } else {
                        tab.claude_config.expanded.insert(section);
                    }
                }
            }
            Event::ClaudeItemSelect(section, idx) => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.claude_config.selected_item = Some((section.clone(), idx));
                    let file_path = match section.as_str() {
                        "skills" => tab
                            .claude_config
                            .skills
                            .get(idx)
                            .map(|i| i.file_path.clone()),
                        "plugins" => tab
                            .claude_config
                            .plugins
                            .get(idx)
                            .map(|i| i.file_path.clone()),
                        "mcp_servers" => tab
                            .claude_config
                            .mcp_servers
                            .get(idx)
                            .map(|i| i.file_path.clone()),
                        "hooks" => tab
                            .claude_config
                            .hooks
                            .get(idx)
                            .map(|i| i.file_path.clone()),
                        "settings" => tab
                            .claude_config
                            .settings
                            .get(idx)
                            .map(|i| i.file_path.clone()),
                        _ => None,
                    };
                    if let Some(path) = file_path {
                        return Task::done(Event::ViewFile(path));
                    }
                }
            }
            Event::NavigateDir(path) => {
                let mut request: Option<(usize, PathBuf)> = None;
                if let Some(tab) = self.active_tab_mut() {
                    tab.current_dir = path.clone();
                    request = Some((tab.id, path));
                }
                if let Some((tab_id, dir)) = request {
                    self.mark_workspaces_dirty();
                    return Self::request_file_tree(tab_id, dir, self.show_hidden);
                }
            }
            Event::NavigateUp => {
                let mut request: Option<(usize, PathBuf)> = None;
                if let Some(tab) = self.active_tab_mut() {
                    if let Some(parent) = tab.current_dir.parent() {
                        // Don't go above repo root
                        if parent.starts_with(&tab.repo_path) || parent == tab.repo_path {
                            let next_dir = parent.to_path_buf();
                            tab.current_dir = next_dir.clone();
                            request = Some((tab.id, next_dir));
                        }
                    }
                }
                if let Some((tab_id, dir)) = request {
                    self.mark_workspaces_dirty();
                    return Self::request_file_tree(tab_id, dir, self.show_hidden);
                }
            }
            Event::ToggleHidden => {
                self.show_hidden = !self.show_hidden;
                self.save_config();
                if let Some(tab) = self.active_tab_mut() {
                    if tab.sidebar_mode == SidebarMode::Files {
                        return Self::request_file_tree(
                            tab.id,
                            tab.current_dir.clone(),
                            self.show_hidden,
                        );
                    }
                }
            }
            Event::DividerDragStart => {
                self.dragging_divider = true;
            }
            Event::DividerDragEnd => {
                if self.dragging_divider {
                    self.dragging_divider = false;
                    self.save_config();
                }
                if self.dragging_console_divider {
                    self.dragging_console_divider = false;
                    self.save_config();
                }
            }
            Event::MouseMoved(x, y) => {
                if self.dragging_divider {
                    // Clamp sidebar width between 150 and 600 pixels (subtract rail width)
                    self.sidebar_width = (x - SPINE_WIDTH).clamp(150.0, 600.0);

                    // Update WebView bounds if active
                    if webview::is_active() {
                        let bounds = self.calculate_webview_bounds();
                        webview::update_bounds(bounds.0, bounds.1, bounds.2, bounds.3);
                    }
                }
                if self.dragging_console_divider {
                    // Console height = distance from bottom of window to mouse position
                    let new_height =
                        (self.window_size.1 - y).clamp(32.0, self.window_size.1 - 140.0);
                    self.console_height = new_height;

                    // Update WebView bounds if active
                    if webview::is_active() {
                        let bounds = self.calculate_webview_bounds();
                        webview::update_bounds(bounds.0, bounds.1, bounds.2, bounds.3);
                    }
                }

                // Edge peek detection — check if cursor is near left/right edge of content area
                if !self.dragging_divider && !self.dragging_console_divider {
                    let content_x = x - SPINE_WIDTH; // x relative to content area
                    let content_width = self.content_viewport_width();
                    let has_left = self.active_workspace_idx > 0;
                    let has_right = self.active_workspace_idx + 1 < self.workspaces.len();

                    let near_left = has_left && (0.0..EDGE_PEEK_ZONE).contains(&content_x);
                    let near_right = has_right
                        && content_x > content_width - EDGE_PEEK_ZONE
                        && content_x <= content_width;

                    if near_left != self.edge_peek_left || near_right != self.edge_peek_right {
                        self.edge_peek_left = near_left;
                        self.edge_peek_right = near_right;
                    }
                }
            }
            Event::ViewFile(path) => {
                let is_dark_theme = self.theme == AppTheme::Dark;
                let is_markdown = TabState::is_markdown_file(&path);
                let is_html = TabState::is_html_file(&path);
                #[cfg(feature = "excalidraw")]
                let has_webview_content =
                    is_markdown || is_html || excalidraw::is_excalidraw_file(&path);
                #[cfg(not(feature = "excalidraw"))]
                let has_webview_content = is_markdown || is_html;
                let mut request: Option<(usize, PathBuf)> = None;

                // Hide WebView if switching to non-webview file
                if !has_webview_content && webview::is_active() {
                    self.hide_webview_for_non_agent();
                }

                if let Some(tab) = self.active_tab_mut() {
                    let requested_signature = file_version_signature(&path);
                    if tab.last_view_request().map(|(p, _)| p) == Some(path.as_path())
                        && tab
                            .last_view_request()
                            .map(|(_, t)| t.elapsed() < Duration::from_millis(350))
                            .unwrap_or(false)
                    {
                        return Task::none();
                    }
                    let same_path = tab.viewing_file_path() == Some(path.as_path());
                    let load_in_progress = tab
                        .file_viewer()
                        .map(|fv| fv.load_in_progress)
                        .unwrap_or(false);
                    let loaded_signature = tab.file_viewer().and_then(|fv| fv.loaded_signature);
                    if same_path && load_in_progress {
                        return Task::none();
                    }
                    if same_path
                        && !load_in_progress
                        && requested_signature.is_some()
                        && loaded_signature == requested_signature
                    {
                        perf_log!(
                            "file_load skip_unchanged tab={} path={}",
                            tab.id,
                            path.display()
                        );
                        return Task::none();
                    }

                    // Clear git selection if any
                    tab.selected_file = None;
                    tab.diff_lines.clear();
                    tab.diff_load_in_progress = false;
                    tab.diff_load_started_at = None;
                    tab.diff_syntax_lines = None;
                    tab.diff_syntax_notice = None;
                    {
                        // Open / replace the file viewer overlay; resets all overlay state.
                        // No-op on agent tabs (which don't host file viewers in v1).
                        if let Some(fv) = tab.open_file_viewer(path.clone()) {
                            fv.load_in_progress = true;
                            fv.load_started_at = Some(Instant::now());
                        }
                    }
                    tab.set_last_view_request(Some(path.clone()), Some(Instant::now()));
                    request = Some((tab.id, path));
                }
                if let Some((tab_id, file_path)) = request {
                    self.mark_log_server_dirty();
                    return Self::request_file_load(tab_id, file_path, is_dark_theme);
                }

                // Inline WebView files (markdown/html/excalidraw) are shown once load completes.
            }
            Event::CloseFileView => {
                // Hide WebView
                self.hide_webview_for_non_agent();

                if let Some(tab) = self.active_tab_mut() {
                    tab.close_file_viewer();
                }
                self.mark_log_server_dirty();
            }
            Event::CopyFileContent => {
                if let Some(tab) = self.active_tab() {
                    if let Some(fv) = tab.file_viewer() {
                        if !fv.file_content.is_empty() {
                            return iced::clipboard::write(fv.file_content.clone());
                        }
                    }
                }
            }
            Event::OpenFileInBrowser => {
                self.mark_log_server_dirty();
                if let Some(tab) = self.active_tab() {
                    if tab
                        .file_viewer()
                        .map(|fv| !fv.file_content.is_empty())
                        .unwrap_or(false)
                    {
                        if let Some(base_url) = self.log_server_state.base_url() {
                            let url = format!("{}/file/{}", base_url, tab.id);
                            let _ = std::process::Command::new("open").arg(&url).spawn();
                        }
                    }
                }
            }
            Event::ToggleTheme => {
                self.theme = self.theme.toggle();
                self.save_config();
                self.recreate_terminals();

                // Re-render current non-image file or active diff so theme-sensitive colors refresh.
                let is_dark = self.theme == AppTheme::Dark;
                if let Some(tab) = self.active_tab_mut() {
                    if let Some(path) = tab.selected_file.clone() {
                        tab.diff_load_in_progress = true;
                        tab.diff_load_started_at = Some(Instant::now());
                        tab.diff_syntax_lines = None;
                        tab.diff_syntax_notice = None;
                        return Self::request_diff(
                            tab.id,
                            tab.repo_path.clone(),
                            path,
                            tab.selected_is_staged,
                            is_dark,
                        );
                    }
                    if let Some(path) = tab.viewing_file_path().map(|p| p.to_path_buf()) {
                        if !TabState::is_image_file(&path) {
                            if let Some(fv) = tab.file_viewer_mut() {
                                fv.load_in_progress = true;
                                fv.load_started_at = Some(Instant::now());
                            }
                            return Self::request_file_load(tab.id, path, is_dark);
                        }
                    }
                }
            }
            Event::ToggleLogServer => {
                let enabled = !self.log_server_enabled;
                self.set_log_server_enabled(enabled);
            }
            Event::IncreaseTerminalFont => {
                let new_size = (self.terminal_font_size + FONT_SIZE_STEP).min(MAX_FONT_SIZE);
                if new_size != self.terminal_font_size {
                    self.terminal_font_size = new_size;
                    self.save_config();
                    self.recreate_terminals();
                }
            }
            Event::DecreaseTerminalFont => {
                let new_size = (self.terminal_font_size - FONT_SIZE_STEP).max(MIN_FONT_SIZE);
                if new_size != self.terminal_font_size {
                    self.terminal_font_size = new_size;
                    self.save_config();
                    self.recreate_terminals();
                }
            }
            Event::ClearTerminal => {
                if let Some(tab) = self.active_tab_mut() {
                    if let Some(term) = tab.terminal_mut() {
                        // Send the clear command to the terminal
                        Self::handle_terminal_backend_command(
                            term,
                            iced_term::backend::Command::Write(b"clear\n".to_vec()),
                            "clear_terminal",
                            true, // user-initiated, always redraw
                        );
                    }
                }
            }
            Event::IncreaseUiFont => {
                let new_size = (self.ui_font_size + FONT_SIZE_STEP).min(MAX_FONT_SIZE);
                if new_size != self.ui_font_size {
                    self.ui_font_size = new_size;
                    self.save_config();
                }
            }
            Event::DecreaseUiFont => {
                let new_size = (self.ui_font_size - FONT_SIZE_STEP).max(MIN_FONT_SIZE);
                if new_size != self.ui_font_size {
                    self.ui_font_size = new_size;
                    self.save_config();
                }
            }
            // Search events
            Event::ToggleSearch => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.search.is_active = !tab.search.is_active;
                    if !tab.search.is_active {
                        // Clear search state when closing
                        tab.search.query.clear();
                        tab.search.matches.clear();
                        tab.search.current_match = 0;
                    }
                }
            }
            Event::SearchQueryChanged(query) => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.search.query = query;
                }
            }
            Event::SearchExecute => {
                if let Some(tab) = self.active_tab_mut() {
                    let query = tab.search.query.clone();
                    let matches = tab.terminal_mut().map(|term| term.search_all(&query));
                    if let Some(matches) = matches {
                        tab.search.matches = matches;
                        tab.search.current_match = 0;

                        // Scroll to first match if found
                        let first_line = tab.search.matches.first().map(|m| m.start.line.0);
                        if let Some(line) = first_line {
                            if let Some(term) = tab.terminal_mut() {
                                term.scroll_to_line(line);
                            }
                        }
                    }
                }
            }
            Event::SearchNext => {
                if let Some(tab) = self.active_tab_mut() {
                    if !tab.search.matches.is_empty() {
                        tab.search.current_match =
                            (tab.search.current_match + 1) % tab.search.matches.len();
                        let line = tab.search.matches[tab.search.current_match].start.line.0;
                        if let Some(term) = tab.terminal_mut() {
                            term.scroll_to_line(line);
                        }
                    }
                }
            }
            Event::SearchPrev => {
                if let Some(tab) = self.active_tab_mut() {
                    if !tab.search.matches.is_empty() {
                        if tab.search.current_match == 0 {
                            tab.search.current_match = tab.search.matches.len() - 1;
                        } else {
                            tab.search.current_match -= 1;
                        }
                        let line = tab.search.matches[tab.search.current_match].start.line.0;
                        if let Some(term) = tab.terminal_mut() {
                            term.scroll_to_line(line);
                        }
                    }
                }
            }
            Event::SearchClose => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.search.is_active = false;
                    tab.search.query.clear();
                    tab.search.matches.clear();
                    tab.search.current_match = 0;
                }
            }
            Event::OpenMarkdownInBrowser => {
                // Write HTML to temp file and open in browser
                if let Some(tab) = self.active_tab() {
                    let html_to_open = tab.file_viewer().and_then(|fv| fv.webview_content.clone());

                    if html_to_open.is_none() {
                        // TODO: This should be async! Reading files synchronously on main thread
                        // can cause UI freezes. For now, skip if no cached content available.
                        freeze_debug!("WARNING: Skipping browser open - no cached content (file would be read on main thread)");
                    }

                    if let Some(html) = &html_to_open {
                        let temp_dir = std::env::temp_dir();
                        let file_name = tab
                            .viewing_file_path()
                            .and_then(|p| p.file_stem())
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| "preview".to_string());
                        let temp_path = temp_dir.join(format!("{}_preview.html", file_name));

                        if std::fs::write(&temp_path, html).is_ok() {
                            // Open in default browser
                            #[cfg(target_os = "macos")]
                            {
                                let _ = std::process::Command::new("open").arg(&temp_path).spawn();
                            }
                            #[cfg(target_os = "linux")]
                            {
                                let _ = std::process::Command::new("xdg-open")
                                    .arg(&temp_path)
                                    .spawn();
                            }
                            #[cfg(target_os = "windows")]
                            {
                                let _ = std::process::Command::new("cmd")
                                    .args(["/C", "start", ""])
                                    .arg(&temp_path)
                                    .spawn();
                            }
                        }
                    } else if let Some(path) = tab.viewing_file_path() {
                        #[cfg(target_os = "macos")]
                        {
                            let _ = std::process::Command::new("open").arg(path).spawn();
                        }
                        #[cfg(target_os = "linux")]
                        {
                            let _ = std::process::Command::new("xdg-open").arg(path).spawn();
                        }
                        #[cfg(target_os = "windows")]
                        {
                            let _ = std::process::Command::new("cmd")
                                .args(["/C", "start", ""])
                                .arg(path)
                                .spawn();
                        }
                    }
                }
            }
            Event::OpenPlansViewer => {
                // If already open, treat the shortcut/menu as a toggle and dismiss.
                if self.webview_kind == WebviewKind::PlansViewer {
                    webview::set_visible(false);
                    self.webview_kind = WebviewKind::None;
                    return Task::none();
                }
                return self.open_plans_viewer(plans_viewer::DocumentSource::Plans, None);
            }
            Event::OpenPlan(source, name) => {
                return self.open_plans_viewer(source, Some(name));
            }
            Event::ClosePlansViewer => {
                if self.webview_kind == WebviewKind::PlansViewer {
                    webview::set_visible(false);
                    self.webview_kind = WebviewKind::None;
                }
            }
            Event::WindowCloseRequested => {
                // Kill all console processes
                for ws in &mut self.workspaces {
                    ws.console.kill_process();
                }
                if self.workspaces_dirty {
                    self.save_workspaces();
                    self.workspaces_dirty = false;
                    self.next_workspace_save_at = None;
                }
                // Signal the log server to shut down
                self.log_server_state.shutdown.notify_one();
                // Clean up instance-specific configuration
                config::cleanup_instance_config();
                // Close the window
                return iced::window::oldest().then(|opt_id| {
                    if let Some(id) = opt_id {
                        iced::window::close(id)
                    } else {
                        iced::exit()
                    }
                });
            }
            Event::WindowUnfocused => {
                self.window_focused = false;
            }
            Event::WindowFocused => {
                self.window_focused = true;

                // Sync all terminals that may have been updated while unfocused
                for tab in self.workspaces.iter_mut().flat_map(|ws| ws.tabs.iter_mut()) {
                    if let Some(term) = tab.terminal_mut() {
                        term.sync_and_redraw();
                    }
                }
                for bt in self
                    .workspaces
                    .iter_mut()
                    .flat_map(|ws| ws.bottom_terminals.iter_mut())
                {
                    if let Some(term) = &mut bt.terminal {
                        term.sync_and_redraw();
                    }
                }

                // Detect wake from sleep: if the last heartbeat was long ago,
                // the system likely slept. Reset git polling and trigger refresh.
                static LAST_FOCUS: std::sync::Mutex<Option<std::time::Instant>> =
                    std::sync::Mutex::new(None);
                let was_sleeping = if let Ok(mut last) = LAST_FOCUS.lock() {
                    let sleeping = last
                        .map(|t| t.elapsed() > Duration::from_secs(300))
                        .unwrap_or(false);
                    *last = Some(std::time::Instant::now());
                    sleeping
                } else {
                    false
                };

                if was_sleeping {
                    freeze_debug!("WAKE RECOVERY: Window focused after sleep, refreshing state");
                    // Reset git polling for active tab so it refreshes quickly
                    if let Some(tab) = self.active_tab_mut() {
                        tab.git_poll_interval_ms = GIT_POLL_FAST_INTERVAL_MS;
                        tab.git_unchanged_streak = 0;
                        tab.last_poll = Instant::now() - Duration::from_secs(60);
                    }
                }
            }
            Event::WindowResized(width, height) => {
                self.window_size = (width, height);
                // Clamp console height to new window bounds
                self.console_height = self.console_height.clamp(32.0, (height - 140.0).max(32.0));

                // Recalculate slide position for new viewport width (snap, no animation)
                let viewport_width = self.content_viewport_width();
                let new_target = self.active_workspace_idx as f32 * viewport_width;
                self.slide_offset = new_target;
                self.slide_target = new_target;
                self.slide_animating = false;
                self.slide_start_time = None;

                let scroll_task = iced::advanced::widget::operate(
                    iced::advanced::widget::operation::scrollable::scroll_to(
                        workspace_scrollable_id(),
                        scrollable::AbsoluteOffset {
                            x: Some(new_target),
                            y: None,
                        },
                    ),
                );

                // Update WebView bounds if active
                if webview::is_active() {
                    let bounds = self.calculate_webview_bounds();
                    webview::update_bounds(bounds.0, bounds.1, bounds.2, bounds.3);
                }

                return scroll_task;
            }
            Event::GitStatusLoaded(snapshot) => {
                if let Some(tab) = self
                    .workspaces
                    .iter_mut()
                    .flat_map(|ws| ws.tabs.iter_mut())
                    .find(|t| t.id == snapshot.tab_id)
                {
                    tab.git_status_loading = false;
                    {
                        // Self-heal: if the worker discovered a different repo root, update
                        if snapshot.repo_path != tab.repo_path && snapshot.is_git_repo {
                            tab.repo_path = snapshot.repo_path.clone();
                            tab.git_poll_interval_ms = GIT_POLL_FAST_INTERVAL_MS;
                            tab.git_unchanged_streak = 0;
                            tab.last_git_status_hash = None;
                        }

                        // Apply the git status from the worker (which already ran
                        // Repository::discover off the main thread)
                        tab.repo_name = snapshot.repo_name;
                        tab.branch_name = snapshot.branch_name;
                        tab.is_git_repo = snapshot.is_git_repo;
                        tab.staged = snapshot.staged;
                        tab.unstaged = snapshot.unstaged;
                        tab.untracked = snapshot.untracked;

                        let effective_hash = git_tab_state_hash(tab);
                        let unchanged = tab.last_git_status_hash == Some(effective_hash);
                        if unchanged {
                            tab.git_unchanged_streak = tab.git_unchanged_streak.saturating_add(1);
                        } else {
                            tab.git_unchanged_streak = 0;
                        }

                        let has_changes = !tab.staged.is_empty()
                            || !tab.unstaged.is_empty()
                            || !tab.untracked.is_empty();
                        tab.git_poll_interval_ms = next_git_poll_interval_ms(
                            tab.is_git_repo,
                            has_changes,
                            tab.git_unchanged_streak,
                        );
                        tab.last_git_status_hash = Some(effective_hash);

                        perf_log!(
                            "git_poll tab={} interval={}ms unchanged={} streak={} changes={}",
                            tab.id,
                            tab.git_poll_interval_ms,
                            unchanged,
                            tab.git_unchanged_streak,
                            tab.staged.len() + tab.unstaged.len() + tab.untracked.len()
                        );

                        tab.last_poll = Instant::now();
                    }
                }
            }
            Event::FileTreeLoaded(snapshot) => {
                if let Some(tab) = self
                    .workspaces
                    .iter_mut()
                    .flat_map(|ws| ws.tabs.iter_mut())
                    .find(|t| t.id == snapshot.tab_id)
                {
                    if tab.current_dir == snapshot.current_dir {
                        tab.file_tree = snapshot.entries;
                    }
                }
            }
            Event::DiffLoaded(snapshot) => {
                if let Some(tab) = self
                    .workspaces
                    .iter_mut()
                    .flat_map(|ws| ws.tabs.iter_mut())
                    .find(|t| t.id == snapshot.tab_id)
                {
                    if tab.selected_file.as_deref() == Some(snapshot.file_path.as_str())
                        && tab.selected_is_staged == snapshot.is_staged
                    {
                        tab.diff_load_in_progress = false;
                        tab.diff_load_started_at = None;
                        tab.diff_lines = snapshot.lines;
                        tab.diff_syntax_lines = snapshot.diff_syntax_lines;
                        tab.diff_syntax_notice = snapshot.diff_syntax_notice;
                    }
                }
            }
            Event::FileLoaded(snapshot) => {
                // Extract WebView HTML before mutable borrow is released
                let mut inline_webview_html: Option<String> = None;
                let mut hide_webview = false;
                let mut syntax_request: Option<(
                    usize,
                    PathBuf,
                    String,
                    Option<FileVersionSignature>,
                    usize,
                )> = None;

                if let Some(tab) = self
                    .workspaces
                    .iter_mut()
                    .flat_map(|ws| ws.tabs.iter_mut())
                    .find(|t| t.id == snapshot.tab_id)
                {
                    if tab.viewing_file_path() == Some(snapshot.path.as_path()) {
                        let loaded_path = snapshot.path.clone();
                        let loaded_signature = snapshot.file_signature;
                        let tab_id = tab.id;
                        let fv = tab.file_viewer_mut().expect("path matched just above");
                        fv.load_in_progress = false;
                        fv.file_content = snapshot.file_content;
                        fv.webview_content = snapshot.webview_content;
                        fv.preview_notice = snapshot.file_preview_notice;
                        fv.syntax_highlight_lines = snapshot.syntax_highlight_lines;
                        fv.syntax_highlight_notice = snapshot.syntax_highlight_notice;
                        fv.syntax_highlight_in_progress = false;
                        fv.syntax_highlight_requested_lines = fv
                            .syntax_highlight_lines
                            .as_ref()
                            .map(|lines| lines.len())
                            .unwrap_or(0);
                        fv.loaded_signature = loaded_signature;
                        fv.image_handle =
                            snapshot.image_path.as_ref().map(image::Handle::from_path);

                        #[cfg(feature = "excalidraw")]
                        let is_excalidraw = excalidraw::is_excalidraw_file(&fv.path);
                        #[cfg(not(feature = "excalidraw"))]
                        let is_excalidraw = false;

                        let is_markdown_webview =
                            TabState::is_markdown_file(&fv.path) && fv.webview_content.is_some();
                        let is_html_webview = TabState::is_html_file(&fv.path);

                        if let Some(html) = &fv.webview_content {
                            if is_excalidraw || is_markdown_webview || is_html_webview {
                                inline_webview_html = Some(html.clone());
                            } else {
                                webview::update_content(html);
                                hide_webview = true;
                            }
                        } else {
                            hide_webview = true;
                        }

                        #[cfg(feature = "excalidraw")]
                        let is_excalidraw_file = excalidraw::is_excalidraw_file(&loaded_path);
                        #[cfg(not(feature = "excalidraw"))]
                        let is_excalidraw_file = false;

                        let is_text_syntax_candidate = fv.webview_content.is_none()
                            && fv.image_handle.is_none()
                            && !fv.file_content.is_empty()
                            && !TabState::is_markdown_file(&loaded_path)
                            && !TabState::is_html_file(&loaded_path)
                            && !is_excalidraw_file;
                        let mut waiting_for_initial_syntax = false;

                        if is_text_syntax_candidate {
                            let total_lines = fv
                                .file_content
                                .lines()
                                .count()
                                .min(MAX_FILE_VIEW_RENDER_LINES);
                            let requested_lines = FILE_SYNTAX_INITIAL_LINES.min(total_lines);
                            if requested_lines > 0 {
                                fv.syntax_highlight_in_progress = true;
                                fv.syntax_highlight_requested_lines = requested_lines;
                                waiting_for_initial_syntax = true;
                                let file_content_clone = fv.file_content.clone();
                                syntax_request = Some((
                                    tab_id,
                                    loaded_path,
                                    file_content_clone,
                                    loaded_signature,
                                    requested_lines,
                                ));
                            }
                        }

                        if !waiting_for_initial_syntax {
                            if let Some(fv) = tab.file_viewer_mut() {
                                fv.load_started_at = None;
                            }
                        }
                    }
                }

                if hide_webview {
                    self.hide_webview_for_non_agent();
                }

                // Show inline WebView after mutable borrow is released
                if let Some(html) = inline_webview_html {
                    let bounds = self.calculate_webview_bounds();
                    self.mark_log_server_dirty();
                    return self.show_webview(html, bounds);
                }

                self.mark_log_server_dirty();
                if let Some((tab_id, path, file_content, file_signature, requested_lines)) =
                    syntax_request
                {
                    return Self::request_file_syntax_highlight(
                        tab_id,
                        path,
                        file_content,
                        self.theme == AppTheme::Dark,
                        file_signature,
                        requested_lines,
                    );
                }
            }
            Event::FileViewScrolled(tab_id, viewport) => {
                let is_dark_theme = self.theme == AppTheme::Dark;
                if let Some(tab) = self
                    .workspaces
                    .iter_mut()
                    .flat_map(|ws| ws.tabs.iter_mut())
                    .find(|t| t.id == tab_id)
                {
                    let Some(fv) = tab.file_viewer() else {
                        return Task::none();
                    };
                    if fv.load_in_progress
                        || fv.syntax_highlight_in_progress
                        || fv.webview_content.is_some()
                        || fv.image_handle.is_some()
                        || fv.file_content.is_empty()
                    {
                        return Task::none();
                    }

                    let current_lines = fv
                        .syntax_highlight_lines
                        .as_ref()
                        .map(|lines| lines.len())
                        .unwrap_or(0);
                    let total_lines = fv
                        .file_content
                        .lines()
                        .count()
                        .min(MAX_FILE_VIEW_RENDER_LINES);
                    if current_lines >= total_lines {
                        return Task::none();
                    }

                    let offset_y = viewport.absolute_offset().y.max(0.0);
                    let visible_start_line = (offset_y / FILE_VIEW_LINE_HEIGHT_ESTIMATE) as usize;
                    let requested_lines = (visible_start_line + FILE_SYNTAX_SCROLL_PREFETCH_LINES)
                        .max(FILE_SYNTAX_INITIAL_LINES)
                        .min(total_lines);

                    if requested_lines <= current_lines
                        || requested_lines <= fv.syntax_highlight_requested_lines
                    {
                        return Task::none();
                    }

                    let view_path = fv.path.clone();
                    let file_content = fv.file_content.clone();
                    let loaded_signature = fv.loaded_signature;
                    let tab_id = tab.id;
                    let fv_mut = tab
                        .file_viewer_mut()
                        .expect("file_viewer was Some just above");
                    fv_mut.syntax_highlight_in_progress = true;
                    fv_mut.syntax_highlight_requested_lines = requested_lines;
                    return Self::request_file_syntax_highlight(
                        tab_id,
                        view_path,
                        file_content,
                        is_dark_theme,
                        loaded_signature,
                        requested_lines,
                    );
                }
            }
            Event::FileSyntaxHighlighted(snapshot) => {
                if let Some(tab) = self
                    .workspaces
                    .iter_mut()
                    .flat_map(|ws| ws.tabs.iter_mut())
                    .find(|t| t.id == snapshot.tab_id)
                {
                    if let Some(fv) = tab.file_viewer_mut() {
                        if fv.path == snapshot.path
                            && fv.loaded_signature == snapshot.file_signature
                        {
                            fv.syntax_highlight_in_progress = false;
                            fv.load_started_at = None;
                            fv.syntax_highlight_requested_lines =
                                fv.syntax_highlight_requested_lines.max(
                                    snapshot
                                        .syntax_highlight_lines
                                        .as_ref()
                                        .map(|lines| lines.len())
                                        .unwrap_or(0),
                                );
                            fv.syntax_highlight_lines = snapshot.syntax_highlight_lines;
                            fv.syntax_highlight_notice = snapshot.syntax_highlight_notice;
                        }
                    }
                }
            }
            Event::LogServerSyncComplete => {
                self.log_server_sync_in_flight = false;
                if self.log_server_sync_queued {
                    self.log_server_sync_queued = false;
                    self.log_server_dirty = true;
                }
                if self.log_server_dirty {
                    return self.queue_log_server_sync();
                }
            }
            Event::SyntectWarmupComplete => {}
            Event::LoadingUiTick => {}
            #[cfg(feature = "stt")]
            Event::SttToggle => {
                if !self.stt_enabled {
                    return Task::none();
                }
                if self.stt_transcribing {
                    // Already transcribing, ignore
                    return Task::none();
                }
                if self.stt_recording {
                    // Stop recording, start transcription
                    self.stt_recording = false;
                    // Drop the stream to stop recording
                    self.stt_stream = None;
                    // Take the buffer
                    let samples = {
                        let mut buf = self.stt_audio_buffer.lock().unwrap();
                        std::mem::take(&mut *buf)
                    };
                    if samples.is_empty() {
                        eprintln!("[STT] Recording stopped with no audio samples");
                        return Task::none();
                    }
                    let duration_secs = samples.len() as f32 / self.stt_sample_rate as f32;
                    let (rms, peak) = stt_audio_stats(&samples);
                    eprintln!(
                        "[STT] Recording stopped: samples={} sample_rate={} duration={:.2}s rms={:.6} peak={:.6}",
                        samples.len(),
                        self.stt_sample_rate,
                        duration_secs,
                        rms,
                        peak
                    );
                    if duration_secs < STT_MIN_RECORDING_SECONDS {
                        eprintln!(
                            "[STT] Ignoring recording shorter than {:.2}s",
                            STT_MIN_RECORDING_SECONDS
                        );
                        return Task::none();
                    }
                    if rms < STT_MIN_RMS && peak < STT_MIN_PEAK {
                        eprintln!(
                            "[STT] Ignoring near-silent recording (rms={:.6}, peak={:.6}); check macOS microphone permission/input device",
                            rms,
                            peak
                        );
                        return Task::none();
                    }
                    self.stt_transcribing = true;
                    // Lazy-init whisper context
                    if self.stt_context.is_none() {
                        let model_path = stt_model_path();
                        if !model_path.exists() {
                            self.stt_transcribing = false;
                            eprintln!(
                                "[STT] Model not found. Download it with:\n  \
                                 mkdir -p ~/.config/gitterm/models && \\\n  \
                                 curl -L -o ~/.config/gitterm/models/ggml-base.en.bin \\\n  \
                                 https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin"
                            );
                            return Task::none();
                        }
                        match whisper_rs::WhisperContext::new_with_params(
                            model_path.to_str().unwrap(),
                            whisper_rs::WhisperContextParameters::default(),
                        ) {
                            Ok(ctx) => {
                                self.stt_context = Some(Arc::new(ctx));
                            }
                            Err(e) => {
                                self.stt_transcribing = false;
                                eprintln!("[STT] Failed to load model: {}", e);
                                return Task::none();
                            }
                        }
                    }
                    let ctx = self.stt_context.clone().unwrap();
                    let sample_rate = self.stt_sample_rate;
                    return Task::perform(
                        async move {
                            tokio::task::spawn_blocking(move || {
                                stt_transcribe(ctx, samples, sample_rate)
                            })
                            .await
                            .unwrap_or_else(|e| Err(format!("Join error: {}", e)))
                        },
                        |result| match result {
                            Ok(text) => Event::SttTranscriptReady(text),
                            Err(e) => Event::SttError(e),
                        },
                    );
                } else {
                    // Start recording
                    match stt_start_recording(self.stt_audio_buffer.clone()) {
                        Ok((stream, sample_rate)) => {
                            self.stt_recording = true;
                            self.stt_stream = Some(stream);
                            self.stt_sample_rate = sample_rate;
                        }
                        Err(e) => {
                            eprintln!("[STT] Failed to start recording: {}", e);
                        }
                    }
                }
            }
            #[cfg(feature = "stt")]
            Event::SttTranscriptReady(text) => {
                self.stt_transcribing = false;
                if !text.is_empty() {
                    // Inject transcribed text into the active tab's terminal
                    if let Some(tab) = self.active_tab_mut() {
                        if let Some(term) = tab.terminal_mut() {
                            Self::handle_terminal_backend_command(
                                term,
                                iced_term::backend::Command::Write(text.into_bytes()),
                                "stt_transcript_inject",
                                true, // user-initiated, always redraw
                            );
                        }
                    }
                }
            }
            #[cfg(feature = "stt")]
            Event::SttError(e) => {
                self.stt_transcribing = false;
                eprintln!("[STT] Error: {}", e);
            }
            Event::WorkspaceSelect(idx) => {
                self.editing_console_command = None;
                if idx < self.workspaces.len() && idx != self.active_workspace_idx {
                    let viewport_width = self.content_viewport_width();
                    let target = idx as f32 * viewport_width;

                    // Start animation from current position
                    self.slide_start_offset = self.slide_offset;
                    self.slide_target = target;
                    self.slide_start_time = Some(Instant::now());
                    self.slide_animating = true;

                    // Update active workspace immediately (tab bar + console switch instantly)
                    self.active_workspace_idx = idx;
                    self.mark_workspaces_dirty();
                    self.sync_plans_dir();

                    // Refresh claude config if active tab is in Claude mode
                    if let Some(tab) = self.active_tab_mut() {
                        if tab.sidebar_mode == SidebarMode::Claude {
                            tab.fetch_claude_config();
                        }
                    }

                    // Set scrollable to starting position for the animation
                    let slide_task = iced::advanced::widget::operate(
                        iced::advanced::widget::operation::scrollable::scroll_to(
                            workspace_scrollable_id(),
                            scrollable::AbsoluteOffset {
                                x: Some(self.slide_start_offset),
                                y: None,
                            },
                        ),
                    );
                    let bar_task = self.scroll_to_active_workspace_bar();
                    if let Some(html) = self.active_inline_webview_html() {
                        let bounds = self.calculate_webview_bounds();
                        return Task::batch([
                            slide_task,
                            bar_task,
                            self.show_webview(html, bounds),
                        ]);
                    }
                    self.hide_webview_for_non_agent();
                    return Task::batch([slide_task, bar_task]);
                }
            }
            Event::SlideAnimationTick => {
                // After user stops swiping, snap to nearest workspace
                if !self.slide_animating {
                    if let Some(last_scroll) = self.last_user_scroll {
                        if last_scroll.elapsed().as_millis() >= SWIPE_DEBOUNCE_MS as u128 {
                            self.last_user_scroll = None;
                            let viewport_width = self.content_viewport_width();
                            let target = self.active_workspace_idx as f32 * viewport_width;
                            if (self.slide_offset - target).abs() > 1.0 {
                                self.slide_start_offset = self.slide_offset;
                                self.slide_target = target;
                                self.slide_start_time = Some(Instant::now());
                                self.slide_animating = true;
                            }
                        }
                    }
                    return Task::none();
                }

                // Animate slide with ease-out cubic
                if let Some(start_time) = self.slide_start_time {
                    let elapsed = start_time.elapsed().as_millis() as f32;
                    let t = (elapsed / SLIDE_DURATION_MS).min(1.0);
                    let eased = 1.0 - (1.0 - t).powi(3);
                    self.slide_offset = self.slide_start_offset
                        + (self.slide_target - self.slide_start_offset) * eased;

                    if t >= 1.0 {
                        self.slide_offset = self.slide_target;
                        self.slide_animating = false;
                        self.slide_start_time = None;
                    }

                    let offset_x = self.slide_offset;
                    return iced::advanced::widget::operate(
                        iced::advanced::widget::operation::scrollable::scroll_to(
                            workspace_scrollable_id(),
                            scrollable::AbsoluteOffset {
                                x: Some(offset_x),
                                y: None,
                            },
                        ),
                    );
                }
            }
            Event::EdgePeekEnter(is_right) => {
                if is_right {
                    self.edge_peek_right = true;
                } else {
                    self.edge_peek_left = true;
                }
            }
            Event::EdgePeekExit => {
                self.edge_peek_left = false;
                self.edge_peek_right = false;
            }
            Event::SlideScrolled(viewport) => {
                // User swiped — track position, debounce snap until they stop
                if !self.slide_animating {
                    let viewport_width = self.content_viewport_width();
                    if viewport_width > 0.0 {
                        let offset = viewport.absolute_offset().x;
                        self.slide_offset = offset;
                        self.last_user_scroll = Some(Instant::now());

                        // Update active workspace based on current scroll position
                        let nearest = ((offset + viewport_width * 0.5) / viewport_width) as usize;
                        let nearest = nearest.min(self.workspaces.len().saturating_sub(1));
                        if nearest != self.active_workspace_idx {
                            self.active_workspace_idx = nearest;
                            self.mark_workspaces_dirty();
                            self.hide_webview_for_non_agent();
                            self.editing_console_command = None;
                            self.sync_plans_dir();
                        }
                    }
                }
            }
            Event::WorkspaceClose(idx) => {
                self.hide_webview_for_non_agent();
                if idx < self.workspaces.len() && self.workspaces.len() > 1 {
                    // Stash the workspace config (env, color, etc.) before removing,
                    // so it's preserved in workspaces.json for reopening later.
                    let ws = &self.workspaces[idx];
                    self.closed_workspace_configs.push(WorkspaceConfig {
                        name: ws.name.clone(),
                        abbrev: ws.abbrev.clone(),
                        dir: ws.dir.to_string_lossy().to_string(),
                        color: ws.color,
                        tabs: Vec::new(), // Don't preserve tabs for closed workspaces
                        run_command: ws.console.run_command.clone(),
                        bottom_terminals: Vec::new(),
                        env: ws.env.clone(),
                        active: false,
                    });
                    // Kill console process before removing workspace
                    self.workspaces[idx].console.kill_process();
                    self.workspaces.remove(idx);
                    if self.active_workspace_idx >= self.workspaces.len() {
                        self.active_workspace_idx = self.workspaces.len() - 1;
                    }
                    self.mark_workspaces_dirty();
                    self.mark_log_server_dirty();
                    self.sync_plans_dir();

                    // Snap slide to new active workspace (no animation)
                    let viewport_width = self.content_viewport_width();
                    let new_target = self.active_workspace_idx as f32 * viewport_width;
                    self.slide_offset = new_target;
                    self.slide_target = new_target;
                    self.slide_animating = false;
                    self.slide_start_time = None;

                    return iced::advanced::widget::operate(
                        iced::advanced::widget::operation::scrollable::scroll_to(
                            workspace_scrollable_id(),
                            scrollable::AbsoluteOffset {
                                x: Some(new_target),
                                y: None,
                            },
                        ),
                    );
                }
            }
            Event::WorkspaceCreate => {
                return Task::perform(
                    async {
                        let folder = rfd::AsyncFileDialog::new()
                            .set_title("Select Workspace Folder")
                            .pick_folder()
                            .await;
                        folder.map(|f| f.path().to_path_buf())
                    },
                    Event::WorkspaceCreated,
                );
            }
            Event::WorkspaceCreated(Some(path)) => {
                let dir_str = path.to_string_lossy().to_string();

                // Check if there's a closed workspace config for this directory
                // and restore its settings (env, color, name, etc.)
                let saved = self
                    .closed_workspace_configs
                    .iter()
                    .position(|c| c.dir == dir_str)
                    .map(|i| self.closed_workspace_configs.remove(i));

                let name = saved.as_ref().map(|c| c.name.clone()).unwrap_or_else(|| {
                    path.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "Workspace".to_string())
                });
                let color = saved.as_ref().map(|c| c.color).unwrap_or_else(|| {
                    let used_colors: Vec<WorkspaceColor> =
                        self.workspaces.iter().map(|ws| ws.color).collect();
                    WorkspaceColor::next_available(&used_colors)
                });
                let mut workspace = Workspace::new(name, path.clone(), color);
                if let Some(ref cfg) = saved {
                    workspace.abbrev = cfg.abbrev.clone();
                    workspace.env = cfg.env.clone();
                    if let Some(cmd) = &cfg.run_command {
                        workspace.console.run_command = Some(cmd.clone());
                        workspace.console.status = ConsoleStatus::Stopped;
                    }
                }
                self.add_tab_to_workspace_with_command(
                    &mut workspace,
                    path,
                    None,
                    Some("claude".to_string()),
                );
                self.workspaces.push(workspace);
                self.active_workspace_idx = self.workspaces.len() - 1;
                self.mark_workspaces_dirty();
                self.mark_log_server_dirty();
                self.sync_plans_dir();

                // Snap slide state to new workspace position
                // (no scroll_to needed — view renders active workspace directly when not animating)
                let viewport_width = self.content_viewport_width();
                let new_target = self.active_workspace_idx as f32 * viewport_width;
                self.slide_offset = new_target;
                self.slide_target = new_target;
                self.slide_animating = false;
                self.slide_start_time = None;
                if let Some((tab_id, repo_path)) = {
                    if let Some(tab) = self.active_tab_mut() {
                        tab.git_status_loading = true;
                        Some((tab.id, tab.repo_path.clone()))
                    } else {
                        None
                    }
                } {
                    return Self::request_git_status(tab_id, repo_path);
                }
            }
            Event::WorkspaceCreated(None) => {}
            Event::WorkspaceSettingsOpen(idx) => {
                self.workspace_settings_open = true;
                self.workspace_settings_idx = idx;
                self.workspace_settings_new_key = String::new();
                self.workspace_settings_new_value = String::new();
            }
            Event::WorkspaceSettingsClose => {
                self.workspace_settings_open = false;
            }
            Event::WorkspaceSettingsNewKeyChanged(s) => {
                self.workspace_settings_new_key = s;
            }
            Event::WorkspaceSettingsNewValueChanged(s) => {
                self.workspace_settings_new_value = s;
            }
            Event::WorkspaceSettingsEnvAdd => {
                let key = self.workspace_settings_new_key.trim().to_string();
                let val = self.workspace_settings_new_value.trim().to_string();
                if !key.is_empty() {
                    let idx = self.workspace_settings_idx;
                    if let Some(ws) = self.workspaces.get_mut(idx) {
                        ws.env.insert(key, val);
                    }
                    self.workspace_settings_new_key = String::new();
                    self.workspace_settings_new_value = String::new();
                    self.workspaces_dirty = true;
                    self.next_workspace_save_at = Some(Instant::now());
                }
            }
            Event::WorkspaceSettingsEnvRemove(key) => {
                let idx = self.workspace_settings_idx;
                if let Some(ws) = self.workspaces.get_mut(idx) {
                    ws.env.remove(&key);
                }
                self.workspaces_dirty = true;
                self.next_workspace_save_at = Some(Instant::now());
            }
            Event::WorkspaceSettingsColorChange(color) => {
                let idx = self.workspace_settings_idx;
                if let Some(ws) = self.workspaces.get_mut(idx) {
                    ws.color = color;
                }
                self.workspaces_dirty = true;
                self.next_workspace_save_at = Some(Instant::now());
            }
            Event::WorkspaceSettingsApplyProfile(profile_name) => {
                let idx = self.workspace_settings_idx;
                if let Some(ws) = self.workspaces.get_mut(idx) {
                    if let Some(profiles) = Self::load_profiles() {
                        if let Some(profile) = profiles.get(&profile_name) {
                            ws.env = profile.clone();
                        }
                    }
                }
                self.workspaces_dirty = true;
                self.next_workspace_save_at = Some(Instant::now());
            }
            // Console panel events
            Event::ConsoleToggle => {
                self.console_expanded = !self.console_expanded;
                self.save_config();
                // Update WebView bounds if active
                if webview::is_active() {
                    let bounds = self.calculate_webview_bounds();
                    webview::update_bounds(bounds.0, bounds.1, bounds.2, bounds.3);
                }
                // When collapsing console while bottom terminal is focused, refocus main terminal
                if !self.console_expanded && self.bottom_panel_focused {
                    return self.focus_main_terminal();
                }
            }
            Event::ConsoleStart => {
                if let Some(ws) = self.active_workspace_mut() {
                    // Use active tab's directory (tracks terminal cwd), fall back to workspace root
                    let dir = ws
                        .active_tab()
                        .map(|t| t.current_dir.clone())
                        .unwrap_or_else(|| ws.dir.clone());
                    ws.console.detected_url = None;
                    let env: Vec<(String, String)> =
                        ws.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    ws.console.spawn_process_with_env(&dir, &env);
                }
                self.console_expanded = true;
            }
            Event::ConsoleStop => {
                if let Some(ws) = self.active_workspace_mut() {
                    ws.console.kill_process();
                    ws.console.status = ConsoleStatus::Stopped;
                }
            }
            Event::ConsoleRestart => {
                if let Some(ws) = self.active_workspace_mut() {
                    ws.console.kill_process();
                    ws.console.detected_url = None;
                    let dir = ws
                        .active_tab()
                        .map(|t| t.current_dir.clone())
                        .unwrap_or_else(|| ws.dir.clone());
                    let env: Vec<(String, String)> =
                        ws.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    ws.console.spawn_process_with_env(&dir, &env);
                }
                self.console_expanded = true;
            }
            Event::ConsoleEditorAction(action) => {
                // Allow selection/navigation but not editing
                if !action.is_edit() {
                    if let Some(ws) = self.active_workspace_mut() {
                        ws.console.editor_content.perform(action);
                    }
                }
            }
            Event::ConsoleSearchToggle => {
                if let Some(ws) = self.active_workspace_mut() {
                    ws.console.search_visible = !ws.console.search_visible;
                    if !ws.console.search_visible {
                        ws.console.search_query.clear();
                        ws.console.rebuild_editor_content();
                    }
                }
            }
            Event::ConsoleSearchChanged(query) => {
                if let Some(ws) = self.active_workspace_mut() {
                    ws.console.search_query = query;
                    ws.console.rebuild_editor_content();
                }
            }
            Event::ConsoleSearchClose => {
                if let Some(ws) = self.active_workspace_mut() {
                    ws.console.search_visible = false;
                    ws.console.search_query.clear();
                    ws.console.rebuild_editor_content();
                }
            }
            Event::ConsoleClearOutput => {
                if let Some(ws) = self.active_workspace_mut() {
                    ws.console.clear_output();
                }
            }
            Event::ConsoleOpenBrowser => {
                if let Some(ws) = self.active_workspace() {
                    if let Some(url) = &ws.console.detected_url {
                        let _ = std::process::Command::new("open").arg(url).spawn();
                    }
                }
            }
            Event::ConsoleDividerDragStart => {
                self.dragging_console_divider = true;
            }
            Event::ConsoleCommandEditStart => {
                let current = self
                    .active_workspace()
                    .and_then(|ws| ws.console.run_command.clone())
                    .unwrap_or_default();
                self.editing_console_command = Some(current);
            }
            Event::ConsoleCommandChanged(val) => {
                self.editing_console_command = Some(val);
            }
            Event::ConsoleCommandSubmit => {
                if let Some(cmd) = self.editing_console_command.take() {
                    if let Some(ws) = self.active_workspace_mut() {
                        if cmd.trim().is_empty() {
                            ws.console.run_command = None;
                            ws.console.status = ConsoleStatus::NoneConfigured;
                        } else {
                            ws.console.run_command = Some(cmd.trim().to_string());
                            if !ws.console.is_running() {
                                ws.console.status = ConsoleStatus::Stopped;
                            }
                        }
                    }
                    self.mark_workspaces_dirty();
                }
            }
            Event::ConsoleCommandCancel => {
                self.editing_console_command = None;
            }
            Event::ModifiersChanged(modifiers) => {
                self.current_modifiers = modifiers;
            }
            Event::ToggleHelp => {
                self.show_help = !self.show_help;
            }
            Event::AttentionPulseTick => {
                self.attention_pulse_bright = !self.attention_pulse_bright;
            }
            Event::AttentionJumpNext => {
                // Round-robin search for next tab needing attention
                let ws_count = self.workspaces.len();
                if ws_count == 0 {
                    return Task::none();
                }
                let start_ws = self.active_workspace_idx;
                let start_tab = self
                    .workspaces
                    .get(start_ws)
                    .map(|ws| ws.active_tab)
                    .unwrap_or(0);

                // Search from (current_ws, current_tab + 1), wrapping around all workspaces/tabs
                let mut ws_idx = start_ws;
                let mut tab_idx = start_tab + 1;
                for _ in 0..(ws_count * 100) {
                    // upper bound to prevent infinite loop
                    if let Some(ws) = self.workspaces.get(ws_idx) {
                        if tab_idx < ws.tabs.len() {
                            if ws.tabs[tab_idx].needs_attention {
                                // Found one — switch to it
                                if ws_idx != self.active_workspace_idx {
                                    // Animate workspace switch
                                    let viewport_width = self.content_viewport_width();
                                    let target = ws_idx as f32 * viewport_width;
                                    self.slide_start_offset = self.slide_offset;
                                    self.slide_target = target;
                                    self.slide_start_time = Some(Instant::now());
                                    self.slide_animating = true;
                                    self.active_workspace_idx = ws_idx;
                                    self.sync_plans_dir();
                                }
                                self.workspaces[ws_idx].active_tab = tab_idx;
                                self.mark_workspaces_dirty();
                                return self.scroll_to_active_tab();
                            }
                            tab_idx += 1;
                            continue;
                        }
                    }
                    // Move to next workspace, first tab
                    ws_idx = (ws_idx + 1) % ws_count;
                    tab_idx = 0;
                    // If we've wrapped back to start and checked past current tab, we're done
                    if ws_idx == start_ws && tab_idx > start_tab {
                        break;
                    }
                }
            }
        }
        Task::none()
    }

    /// Hide the singleton webview unless it's the agent webview for the
    /// currently active tab — in which case it's the tab's primary content
    /// and should stay visible. Replaces unconditional `webview::set_visible(false)`
    /// calls scattered across handlers that predate the agent tab kind.
    fn hide_webview_for_non_agent(&mut self) {
        let active_is_agent_tab_with_active_webview = self
            .active_tab()
            .map(|t| matches!(t.kind, TabKind::Agent(_)))
            .unwrap_or(false)
            && self.webview_kind == WebviewKind::Agent;
        if !active_is_agent_tab_with_active_webview {
            webview::set_visible(false);
            // Clear kind for any non-agent surface so the plans-viewer header
            // (and any future kind-driven chrome) goes away with the webview.
            if self.webview_kind != WebviewKind::Agent {
                self.webview_kind = WebviewKind::None;
            }
        }
    }

    /// Calculate WebView bounds based on current layout
    fn calculate_webview_bounds(&self) -> (f32, f32, f32, f32) {
        let tab_bar_height = 33.0; // top tab strip
        let file_header_height = 40.0; // file viewer header row (Back/View in Browser/Close)
        let workspace_bar_height = 28.0; // bottom workspace bar + 1px border
        let x = if self.sidebar_collapsed {
            SPINE_WIDTH + 36.0 + 1.0 // spine + icon rail + border
        } else {
            SPINE_WIDTH + self.sidebar_width + 4.0 // spine + sidebar + divider
        };
        let width = (self.window_size.0 - x).max(100.0);

        // Subtract console panel height + workspace bar (reserved at the bottom)
        let console_h = if self.console_expanded {
            self.console_height + CONSOLE_DIVIDER_HEIGHT
        } else {
            CONSOLE_HEADER_HEIGHT
        };

        // wry child-WebView coordinates behave bottom-origin on macOS in this app.
        // Place the child above bottom panels and reserve top chrome (tabs + file header).
        let y = console_h + workspace_bar_height;
        let top_reserved = tab_bar_height + file_header_height;
        let height = (self.window_size.1 - y - top_reserved).max(100.0);
        (x, y, width, height)
    }

    /// Create or update the embedded WebView with static HTML content
    /// (markdown, excalidraw, html file viewer). If the webview is currently
    /// hosting agent chat, we destroy + recreate it (the chat IPC handler
    /// can't be swapped post-construction).
    fn show_webview(&mut self, html: String, bounds: (f32, f32, f32, f32)) -> Task<Event> {
        perf_log!(
            "webview mode={} html_bytes={} bounds=({}, {}, {}, {})",
            if webview::is_active() {
                "reuse"
            } else {
                "create"
            },
            html.len(),
            bounds.0,
            bounds.1,
            bounds.2,
            bounds.3
        );
        // If the active webview is the agent chat, tear it down so we don't
        // try to render markdown into the chat scaffold (or vice-versa).
        if self.webview_kind == WebviewKind::Agent {
            webview::destroy();
            self.webview_kind = WebviewKind::None;
            self.webview_agent_tab_id = None;
        }
        // Reuse the existing WebView when possible to avoid expensive recreation churn.
        if webview::is_active() {
            webview::update_bounds(bounds.0, bounds.1, bounds.2, bounds.3);
            webview::update_content(&html);
            webview::set_visible(true);
            self.webview_kind = WebviewKind::Static;
            return Task::none();
        }

        webview::set_pending_content(html, bounds);
        self.webview_kind = WebviewKind::Static;
        iced::window::oldest().then(|opt_id| {
            if let Some(id) = opt_id {
                iced::window::run(id, |window| {
                    if let Err(e) = webview::try_create_with_window(window) {
                        perf_log!("webview_create_failed: {e}");
                    }
                })
                .discard()
            } else {
                Task::none()
            }
        })
    }

    /// Build the markdown-viewer URL and navigate the in-app webview to it.
    /// If `plan` is `Some`, the viewer boots straight to that file via
    /// `?plan=<name>`. Returns `Task::none()` if the log server isn't up
    /// (with a hint logged to stderr).
    fn open_plans_viewer(
        &mut self,
        source: plans_viewer::DocumentSource,
        plan: Option<String>,
    ) -> Task<Event> {
        let Some(base_url) = self.log_server_state.base_url() else {
            eprintln!(
                "[plans-viewer] cannot open: log server not running (View → Toggle Local Log Server)"
            );
            return Task::none();
        };
        let theme_str = match self.theme {
            AppTheme::Dark => "dark",
            AppTheme::Light => "light",
        };
        // nav=hide tells the viewer to drop its built-in plan list (gitterm's
        // sidebar already shows plans). TOC stays — it's per-document.
        let route = source.route_segment();
        let url = match plan {
            Some(name) => format!(
                "{}/{}/viewer?theme={}&nav=hide&plan={}",
                base_url,
                route,
                theme_str,
                urlencoding_simple(&name)
            ),
            None => format!("{}/{}/viewer?theme={}&nav=hide", base_url, route, theme_str),
        };
        let bounds = self.calculate_webview_bounds();
        self.show_webview_url(url, bounds)
    }

    /// Create or update the embedded WebView to display a URL (used by the
    /// plans viewer, which loads `/plans/viewer` from the local warp server).
    /// Mirrors `show_webview` but stages a URL instead of HTML.
    fn show_webview_url(&mut self, url: String, bounds: (f32, f32, f32, f32)) -> Task<Event> {
        perf_log!(
            "webview_url mode={} url={} bounds=({}, {}, {}, {})",
            if webview::is_active() {
                "reuse"
            } else {
                "create"
            },
            url,
            bounds.0,
            bounds.1,
            bounds.2,
            bounds.3
        );
        if self.webview_kind == WebviewKind::Agent {
            webview::destroy();
            self.webview_kind = WebviewKind::None;
            self.webview_agent_tab_id = None;
        }
        if webview::is_active() {
            webview::update_bounds(bounds.0, bounds.1, bounds.2, bounds.3);
            webview::navigate_to_url(&url);
            webview::set_visible(true);
            self.webview_kind = WebviewKind::PlansViewer;
            return Task::none();
        }

        webview::set_pending_url(url, bounds);
        self.webview_kind = WebviewKind::PlansViewer;
        iced::window::oldest().then(|opt_id| {
            if let Some(id) = opt_id {
                iced::window::run(id, |window| {
                    if let Err(e) = webview::try_create_with_window(window) {
                        perf_log!("webview_create_failed: {e}");
                    }
                })
                .discard()
            } else {
                Task::none()
            }
        })
    }

    /// Show the agent chat webview for `tab_id`. If the webview is currently
    /// hosting static content (markdown / excalidraw), destroy + recreate it
    /// with the chat HTML and IPC handler. If it's already an agent webview
    /// for a different tab, just clear and replay this tab's buffer.
    fn show_agent_webview(&mut self, tab_id: usize) -> Task<Event> {
        let bounds = self.calculate_webview_bounds();

        // Find the conversation buffer to replay.
        let conversation: Vec<tab::AgentEvent> = self
            .workspaces
            .iter()
            .flat_map(|ws| ws.tabs.iter())
            .find(|t| t.id == tab_id)
            .and_then(|t| t.agent_session())
            .map(|s| s.conversation.clone())
            .unwrap_or_default();

        // Reconcile bookkeeping with the actual webview state. If we think we
        // have an Agent webview but `is_active()` says no, the previous
        // construction must have failed silently (or been destroyed by a
        // sibling code path). Fall through to the recreate branch below.
        let webview_alive = webview::is_active();
        let same_tab_fast_path = self.webview_kind == WebviewKind::Agent
            && self.webview_agent_tab_id == Some(tab_id)
            && webview_alive;
        let other_agent_tab_path = self.webview_kind == WebviewKind::Agent
            && self.webview_agent_tab_id != Some(tab_id)
            && webview_alive;

        eprintln!(
            "[agent-webview] show tab={} kind={:?} alive={} same={} other={} bounds=({},{},{},{}) buf_len={}",
            tab_id,
            self.webview_kind,
            webview_alive,
            same_tab_fast_path,
            other_agent_tab_path,
            bounds.0,
            bounds.1,
            bounds.2,
            bounds.3,
            conversation.len()
        );

        if same_tab_fast_path {
            webview::update_bounds(bounds.0, bounds.1, bounds.2, bounds.3);
            webview::set_visible(true);
            return Task::none();
        }

        if other_agent_tab_path {
            // Different agent tab on the live agent webview — reset surface
            // and replay this tab's events. No recreate needed (the IPC
            // handler routes by tabId, set per activation).
            webview::update_bounds(bounds.0, bounds.1, bounds.2, bounds.3);
            webview::set_visible(true);
            reset_agent_webview();
            set_agent_webview_tab_id(tab_id);
            for ev in &conversation {
                push_agent_event_to_webview(ev);
            }
            self.webview_agent_tab_id = Some(tab_id);
            return Task::none();
        }

        // No agent webview alive — destroy whatever's there and recreate.
        if webview_alive {
            eprintln!(
                "[agent-webview] destroying existing kind={:?}",
                self.webview_kind
            );
            webview::destroy();
        }
        webview::set_pending_content_with_ipc(
            AGENT_CHAT_HTML.to_string(),
            bounds,
            Some(agent_ipc_handler()),
        );
        self.webview_kind = WebviewKind::Agent;
        self.webview_agent_tab_id = Some(tab_id);

        // After construction, set the tab id on the JS side and replay buffered
        // events. Both happen via evaluate_script after `try_create_with_window`
        // runs. Using Arc because Task::then's closure is FnMut, so the Vec
        // can't be moved out of the captured environment on each call.
        let conversation = std::sync::Arc::new(conversation);
        iced::window::oldest().then(move |opt_id| {
            let conversation = std::sync::Arc::clone(&conversation);
            if let Some(id) = opt_id {
                iced::window::run(id, move |window| {
                    eprintln!("[agent-webview] try_create_with_window for tab={}", tab_id);
                    if let Err(e) = webview::try_create_with_window(window) {
                        eprintln!("[agent-webview] create FAILED for tab={}: {}", tab_id, e);
                        return;
                    }
                    eprintln!(
                        "[agent-webview] create OK for tab={}, replaying {} events",
                        tab_id,
                        conversation.len()
                    );
                    set_agent_webview_tab_id(tab_id);
                    for ev in conversation.iter() {
                        push_agent_event_to_webview(ev);
                    }
                })
                .discard()
            } else {
                eprintln!("[agent-webview] no window available; skipping create");
                Task::none()
            }
        })
    }

    fn recreate_terminals(&mut self) {
        let started = Instant::now();
        // Pre-compute settings params to avoid borrow conflict with iter_mut
        let scrollback = self.scrollback_lines;
        let theme = self.theme;
        let font_size = self.terminal_font_size;
        let mut recreated_main = 0usize;
        let mut recreated_bottom = 0usize;

        for tab in self.workspaces.iter_mut().flat_map(|ws| ws.tabs.iter_mut()) {
            let settings = Self::build_terminal_settings(
                &tab.repo_path,
                None,
                scrollback,
                &theme,
                font_size,
                &[],
            );
            if let Ok(mut terminal) = iced_term::Terminal::new(tab.id as u64, settings) {
                terminal.handle(iced_term::Command::AddBindings(
                    Self::standard_noop_bindings(),
                ));
                tab.set_terminal(terminal);
                tab.created_at = Instant::now();
                recreated_main += 1;
            }
        }

        // Recreate bottom panel terminals
        for ws in self.workspaces.iter_mut() {
            for bt in ws.bottom_terminals.iter_mut() {
                let settings = Self::build_terminal_settings(
                    &bt.cwd,
                    None,
                    scrollback,
                    &theme,
                    font_size,
                    &[],
                );
                bt.terminal = iced_term::Terminal::new(bt.id as u64, settings)
                    .ok()
                    .map(|mut t| {
                        t.handle(iced_term::Command::AddBindings(
                            Self::standard_noop_bindings(),
                        ));
                        recreated_bottom += 1;
                        t
                    });
            }
        }

        let elapsed = started.elapsed();
        if elapsed > Duration::from_millis(25) {
            freeze_debug!(
                "recreate_terminals took {}ms (main={}, bottom={})",
                elapsed.as_millis(),
                recreated_main,
                recreated_bottom
            );
        }
    }

    fn view(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        heartbeat("view");
        let spine = freeze_time!("view_spine", { self.view_spine() });
        let tab_bar = freeze_time!("view_tab_bar", { self.view_tab_bar() });
        let content = freeze_time!("view_workspace_slide", { self.view_workspace_slide() });
        let console_panel = freeze_time!("view_bottom_panel", { self.view_bottom_panel() });

        let mut main_col = Column::new()
            .spacing(0)
            .width(Length::Fill)
            .height(Length::Fill);
        main_col = main_col.push(tab_bar);
        if self.webview_kind == WebviewKind::PlansViewer {
            main_col = main_col.push(self.view_plans_viewer_header());
        }
        main_col = main_col.push(content);

        // Console divider (only when expanded)
        if self.console_expanded {
            let theme = &self.theme;
            let divider_color = if self.dragging_console_divider {
                theme.accent()
            } else {
                theme.surface0()
            };
            let console_divider = iced::widget::mouse_area(
                container(iced::widget::Space::new())
                    .width(Length::Fill)
                    .height(Length::Fixed(CONSOLE_DIVIDER_HEIGHT))
                    .style(move |_| container::Style {
                        background: Some(divider_color.into()),
                        ..Default::default()
                    }),
            )
            .on_press(Event::ConsoleDividerDragStart)
            .interaction(iced::mouse::Interaction::ResizingVertically);
            main_col = main_col.push(console_divider);
        }

        main_col = main_col.push(console_panel);

        // Bottom workspace bar
        let workspace_bar = self.view_workspace_bar();
        main_col = main_col.push(workspace_bar);

        let main_view: Element<'_, Event, Theme, iced::Renderer> = row![spine, main_col]
            .spacing(0)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();

        if self.show_help {
            Stack::new()
                .push(main_view)
                .push(self.view_help_modal())
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else if self.workspace_settings_open {
            Stack::new()
                .push(main_view)
                .push(self.view_workspace_settings_modal())
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else if self.tab_picker_visible {
            Stack::new()
                .push(main_view)
                .push(self.view_tab_picker())
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else {
            main_view
        }
    }

    /// Header strip shown above the embedded webview while the plans viewer
    /// is active. Mirrors the file-viewer header so the existing 40px
    /// reservation in `calculate_webview_bounds` lines up.
    fn view_plans_viewer_header(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let font_small = self.ui_font_small();
        let header_bg = theme.bg_overlay();
        let ghost = self.ghost_button_style();
        let header = row![
            text("Plans Viewer").size(font).color(theme.text_primary()),
            iced::widget::Space::new().width(Length::Fill),
            text("Esc: close")
                .size(font_small)
                .color(theme.text_secondary()),
            iced::widget::Space::new().width(Length::Fixed(8.0)),
            button(text("Back to Terminal").size(font))
                .style(ghost)
                .padding([4, 12])
                .on_press(Event::ClosePlansViewer),
        ]
        .padding(8)
        .spacing(8);
        container(header)
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(header_bg.into()),
                ..Default::default()
            })
            .into()
    }

    fn view_tab_picker(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let bg = theme.bg_surface();
        let border_color = theme.border();
        let text_primary = theme.text_primary();
        let text_secondary = theme.text_secondary();
        let hover_bg = theme.surface0();
        let mono = iced::Font::with_name("Menlo");

        let picker_row = |label: String,
                          desc: String,
                          icon: String,
                          accent: iced::Color,
                          event: Event|
         -> Element<'_, Event, Theme, iced::Renderer> {
            let hover = hover_bg;
            button(
                row![
                    text(icon)
                        .size(14)
                        .color(accent)
                        .font(mono)
                        .width(Length::Fixed(24.0)),
                    column![
                        text(label).size(13).color(text_primary),
                        text(desc).size(11).color(text_secondary),
                    ]
                    .spacing(1)
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center)
                .padding([6, 10]),
            )
            .style(move |_theme, status| {
                let bg_color = if matches!(status, button::Status::Hovered) {
                    Some(hover.into())
                } else {
                    None
                };
                button::Style {
                    background: bg_color,
                    text_color: text_primary,
                    border: iced::Border::default(),
                    ..Default::default()
                }
            })
            .padding(0)
            .width(Length::Fill)
            .on_press(event)
            .into()
        };

        let mut items = Column::new().spacing(0).width(Length::Fixed(240.0));
        for (idx, preset) in self.agent_presets.iter().enumerate() {
            let icon = if preset.icon.is_empty() {
                preset.name.chars().next().unwrap_or('?').to_string()
            } else {
                preset.icon.clone()
            };
            let accent = preset.color.color(theme);
            items = items.push(picker_row(
                preset.name.clone(),
                preset.command.clone(),
                icon.clone(),
                accent,
                Event::LaunchAgentPreset(idx),
            ));
            // Add resume row if the preset has a resume command
            if let Some(resume_cmd) = &preset.resume_command {
                items = items.push(picker_row(
                    format!("{} (resume)", preset.name),
                    resume_cmd.clone(),
                    "\u{21ba}".to_string(), // ↺ symbol
                    accent,
                    Event::ResumeAgentPreset(idx),
                ));
            }
        }
        // Always add plain terminal at the bottom
        items = items.push(picker_row(
            "Terminal".to_string(),
            "Plain shell".to_string(),
            "\u{25b8}".to_string(),
            text_secondary,
            Event::NewPlainTab,
        ));

        let picker_menu = container(items)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                border: iced::Border {
                    color: border_color,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                shadow: iced::Shadow {
                    color: iced::Color::from_rgba(0.0, 0.0, 0.0, 0.3),
                    offset: iced::Vector::new(0.0, 2.0),
                    blur_radius: 8.0,
                },
                ..Default::default()
            })
            .padding(4);

        // Click-away backdrop to dismiss
        let backdrop = iced::widget::mouse_area(
            container(iced::widget::Space::new())
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .on_press(Event::HideTabPicker);

        // Position the picker near top-left (below tab bar area)
        Stack::new()
            .push(backdrop)
            .push(
                container(picker_menu)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(iced::alignment::Horizontal::Left)
                    .align_y(iced::alignment::Vertical::Top)
                    .padding(iced::Padding {
                        top: 32.0,
                        right: 0.0,
                        bottom: 0.0,
                        left: 56.0,
                    }),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn view_help_modal(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let accent = theme.accent();
        let text_primary = theme.text_primary();
        let text_secondary = theme.text_secondary();
        let text_muted = theme.text_muted();
        let bg_surface = theme.bg_surface();
        let border_color = theme.border();
        let bg_crust = theme.bg_crust();

        let mono = iced::Font::with_name("Menlo");

        // Helper to build a shortcut row
        let shortcut_row = |key_str: &'static str,
                            desc_str: &'static str|
         -> Element<'_, Event, Theme, iced::Renderer> {
            row![
                container(text(key_str).size(13).color(text_primary).font(mono))
                    .width(Length::Fixed(180.0)),
                text(desc_str).size(13).color(text_secondary),
            ]
            .spacing(12)
            .align_y(iced::Alignment::Center)
            .into()
        };

        let section_header = |title: &'static str| -> Element<'_, Event, Theme, iced::Renderer> {
            container(text(title).size(12).color(accent).font(mono))
                .padding(iced::Padding {
                    top: 8.0,
                    right: 0.0,
                    bottom: 4.0,
                    left: 0.0,
                })
                .into()
        };

        let mut content_col = Column::new().spacing(2).padding([24, 32]);

        // Title
        content_col = content_col.push(
            container(text("Keyboard Shortcuts").size(18).color(text_primary)).padding(
                iced::Padding {
                    top: 0.0,
                    right: 0.0,
                    bottom: 12.0,
                    left: 0.0,
                },
            ),
        );

        // Navigation
        content_col = content_col.push(section_header("Navigation"));
        content_col = content_col.push(shortcut_row("Ctrl + 1-9", "Switch workspace"));
        content_col = content_col.push(shortcut_row("Cmd + 1-9", "Switch tab"));
        content_col = content_col.push(shortcut_row("Ctrl + `", "Jump to attention tab"));
        content_col = content_col.push(shortcut_row("Cmd + Shift + W", "Close workspace"));
        content_col = content_col.push(shortcut_row("Cmd + B", "Toggle sidebar"));

        // Tabs
        content_col = content_col.push(section_header("Tabs"));
        content_col = content_col.push(shortcut_row("+ button", "New Claude tab"));
        content_col = content_col.push(shortcut_row(
            "Option + Click +",
            "Tab picker (Claude/Codex/Terminal)",
        ));
        content_col = content_col.push(shortcut_row("Option + Shift + C", "Resume Claude session"));
        content_col = content_col.push(shortcut_row("Option + Shift + X", "New Codex tab"));
        content_col = content_col.push(shortcut_row("Option + Shift + T", "New terminal (folder)"));

        // Console
        content_col = content_col.push(section_header("Console"));
        content_col = content_col.push(shortcut_row("Cmd + J", "Toggle bottom panel"));
        content_col = content_col.push(shortcut_row("Cmd + Shift + R", "Restart console"));

        // Terminal
        content_col = content_col.push(section_header("Terminal"));
        content_col = content_col.push(shortcut_row("Cmd + K", "Clear terminal"));
        content_col = content_col.push(shortcut_row("Cmd + F", "Find in terminal"));
        content_col = content_col.push(shortcut_row("Cmd + G", "Next match"));
        content_col = content_col.push(shortcut_row("Cmd + Shift + G", "Previous match"));

        // Font Size
        content_col = content_col.push(section_header("Font Size"));
        content_col = content_col.push(shortcut_row("Cmd + =", "Increase terminal font"));
        content_col = content_col.push(shortcut_row("Cmd + -", "Decrease terminal font"));
        content_col = content_col.push(shortcut_row("Cmd + Shift + =", "Increase UI font"));
        content_col = content_col.push(shortcut_row("Cmd + Shift + -", "Decrease UI font"));

        // Theme
        content_col = content_col.push(section_header("Theme"));
        content_col = content_col.push(shortcut_row("Cmd + Shift + T", "Toggle light/dark"));

        // Footer
        content_col = content_col.push(
            container(
                text("Press Option+/ or Esc to close")
                    .size(12)
                    .color(text_muted),
            )
            .padding(iced::Padding {
                top: 12.0,
                right: 0.0,
                bottom: 0.0,
                left: 0.0,
            }),
        );

        // Card
        let card = container(content_col)
            .max_width(460)
            .style(move |_| container::Style {
                background: Some(bg_surface.into()),
                border: iced::Border {
                    color: border_color,
                    width: 1.0,
                    radius: 8.0.into(),
                },
                ..Default::default()
            });

        // Backdrop — semi-transparent overlay with centered card
        let backdrop_color = iced::Color { a: 0.8, ..bg_crust };
        container(
            container(card)
                .center_x(Length::Fill)
                .center_y(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_| container::Style {
            background: Some(backdrop_color.into()),
            ..Default::default()
        })
        .into()
    }

    fn view_workspace_settings_modal(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let text_primary = theme.text_primary();
        let text_secondary = theme.text_secondary();
        let text_muted = theme.text_muted();
        let bg_surface = theme.bg_surface();
        let bg_base = theme.bg_base();
        let border_color = theme.border();
        let bg_crust = theme.bg_crust();
        let accent = theme.accent();
        let danger = theme.danger();
        let mono = iced::Font::with_name("Menlo");

        let idx = self.workspace_settings_idx;
        let ws = match self.workspaces.get(idx) {
            Some(w) => w,
            None => {
                return container(iced::widget::Space::new())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into();
            }
        };

        // --- Title ---
        let title = text(format!("Workspace: {}", ws.name))
            .size(16)
            .color(text_primary);

        // --- Color swatches ---
        let mut color_row = Row::new().spacing(6).align_y(iced::Alignment::Center);
        color_row = color_row.push(text("Color:").size(12).color(text_secondary).font(mono));
        for c in WorkspaceColor::ALL {
            let swatch_color = c.color(theme);
            let is_selected = ws.color == c;
            let border_width = if is_selected { 2.0 } else { 0.0 };
            let border_c = if is_selected {
                accent
            } else {
                iced::Color::TRANSPARENT
            };
            color_row = color_row.push(
                button(iced::widget::Space::new())
                    .width(Length::Fixed(18.0))
                    .height(Length::Fixed(18.0))
                    .style(move |_, _| button::Style {
                        background: Some(swatch_color.into()),
                        border: iced::Border {
                            color: border_c,
                            width: border_width,
                            radius: 9.0.into(),
                        },
                        ..Default::default()
                    })
                    .on_press(Event::WorkspaceSettingsColorChange(c)),
            );
        }

        // --- Profile selector ---
        let profiles = Self::load_profiles();
        let mut profile_row = Row::new().spacing(6).align_y(iced::Alignment::Center);
        profile_row = profile_row.push(text("Profile:").size(12).color(text_secondary).font(mono));

        if let Some(ref profiles) = profiles {
            // Detect which profile is currently active (all keys match)
            let current_profile = profiles.iter().find_map(|(name, env)| {
                if !env.is_empty() && env.iter().all(|(k, v)| ws.env.get(k) == Some(v)) {
                    Some(name.as_str())
                } else {
                    None
                }
            });

            let mut profile_names: Vec<&String> = profiles.keys().collect();
            profile_names.sort();

            for name in profile_names {
                let is_active = current_profile == Some(name.as_str());
                let name_label = name.clone();
                let name_event = name.clone();
                let btn_accent = accent;
                let btn_border = if is_active { accent } else { border_color };
                let btn_bg = if is_active {
                    accent
                } else {
                    iced::Color::TRANSPARENT
                };
                let btn_text_color = if is_active { bg_base } else { text_primary };

                profile_row = profile_row.push(
                    button(text(name_label).size(11).color(btn_text_color).font(mono))
                        .style(move |_, status| button::Style {
                            background: Some(
                                if is_active {
                                    btn_bg
                                } else if matches!(status, button::Status::Hovered) {
                                    btn_accent
                                } else {
                                    btn_bg
                                }
                                .into(),
                            ),
                            text_color: if !is_active && matches!(status, button::Status::Hovered) {
                                bg_base
                            } else {
                                btn_text_color
                            },
                            border: iced::Border {
                                color: btn_border,
                                width: 1.0,
                                radius: 3.0.into(),
                            },
                            ..Default::default()
                        })
                        .padding([3, 10])
                        .on_press(Event::WorkspaceSettingsApplyProfile(name_event)),
                );
            }
        } else {
            profile_row = profile_row.push(
                text("No profiles.json found")
                    .size(11)
                    .color(text_muted)
                    .font(mono),
            );
        }

        // --- Env vars table ---
        let section_label = text("Environment Variables")
            .size(12)
            .color(accent)
            .font(mono);

        let mut env_col = Column::new().spacing(4);

        // Header row
        env_col = env_col.push(
            row![
                container(text("KEY").size(11).color(text_muted).font(mono))
                    .width(Length::Fixed(180.0)),
                text("VALUE").size(11).color(text_muted).font(mono),
            ]
            .spacing(8),
        );

        // Existing entries (sorted for stable display)
        let mut env_entries: Vec<(&String, &String)> = ws.env.iter().collect();
        env_entries.sort_by_key(|(k, _)| k.as_str());

        for (key, val) in env_entries {
            let k = key.clone();
            let remove_btn = button(text("✕").size(10).color(danger).font(mono))
                .style(move |_, _| button::Style {
                    background: Some(iced::Color::TRANSPARENT.into()),
                    ..Default::default()
                })
                .padding([1, 4])
                .on_press(Event::WorkspaceSettingsEnvRemove(k));

            env_col = env_col.push(
                row![
                    container(text(key.as_str()).size(12).color(text_primary).font(mono))
                        .width(Length::Fixed(180.0)),
                    text(val.as_str()).size(12).color(text_secondary).font(mono),
                    iced::widget::Space::new().width(Length::Fill),
                    remove_btn,
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            );
        }

        // Add new row
        let input_style = move |_theme: &Theme, _status: text_input::Status| text_input::Style {
            background: bg_base.into(),
            border: iced::Border {
                width: 1.0,
                color: border_color,
                radius: 3.0.into(),
            },
            icon: iced::Color::TRANSPARENT,
            placeholder: theme.overlay0(),
            value: text_primary,
            selection: accent,
        };

        let key_input = text_input("KEY", &self.workspace_settings_new_key)
            .on_input(Event::WorkspaceSettingsNewKeyChanged)
            .on_submit(Event::WorkspaceSettingsEnvAdd)
            .size(12)
            .width(Length::Fixed(174.0))
            .padding([4, 6])
            .style(input_style);

        let val_input = text_input("VALUE", &self.workspace_settings_new_value)
            .on_input(Event::WorkspaceSettingsNewValueChanged)
            .on_submit(Event::WorkspaceSettingsEnvAdd)
            .size(12)
            .width(Length::Fixed(200.0))
            .padding([4, 6])
            .style(input_style);

        let add_btn = button(text("Add").size(12).color(accent).font(mono))
            .style(move |_, status| button::Style {
                background: Some(
                    if matches!(status, button::Status::Hovered) {
                        theme.surface0()
                    } else {
                        iced::Color::TRANSPARENT
                    }
                    .into(),
                ),
                border: iced::Border {
                    color: accent,
                    width: 1.0,
                    radius: 3.0.into(),
                },
                ..Default::default()
            })
            .padding([4, 10])
            .on_press(Event::WorkspaceSettingsEnvAdd);

        env_col = env_col.push(
            row![key_input, val_input, add_btn]
                .spacing(8)
                .align_y(iced::Alignment::Center),
        );

        // --- Close button ---
        let close_btn = button(text("Close").size(12).color(text_secondary).font(mono))
            .style(move |_, status| button::Style {
                background: Some(
                    if matches!(status, button::Status::Hovered) {
                        bg_surface
                    } else {
                        iced::Color::TRANSPARENT
                    }
                    .into(),
                ),
                border: iced::Border {
                    color: border_color,
                    width: 1.0,
                    radius: 3.0.into(),
                },
                ..Default::default()
            })
            .padding([4, 14])
            .on_press(Event::WorkspaceSettingsClose);

        let note = text("Changes apply to new terminal sessions")
            .size(11)
            .color(text_muted);

        // --- Compose card ---
        let card_content = column![
            title,
            container(iced::widget::Space::new()).height(Length::Fixed(12.0)),
            color_row,
            container(iced::widget::Space::new()).height(Length::Fixed(12.0)),
            profile_row,
            container(iced::widget::Space::new()).height(Length::Fixed(16.0)),
            section_label,
            container(iced::widget::Space::new()).height(Length::Fixed(6.0)),
            scrollable(env_col).height(Length::Fixed(200.0)),
            container(iced::widget::Space::new()).height(Length::Fixed(12.0)),
            row![
                note,
                iced::widget::Space::new().width(Length::Fill),
                close_btn
            ]
            .align_y(iced::Alignment::Center),
        ]
        .padding([24, 28])
        .spacing(0);

        let card = container(card_content)
            .max_width(560)
            .style(move |_| container::Style {
                background: Some(bg_surface.into()),
                border: iced::Border {
                    color: border_color,
                    width: 1.0,
                    radius: 8.0.into(),
                },
                ..Default::default()
            });

        let backdrop_color = iced::Color { a: 0.8, ..bg_crust };
        container(
            container(card)
                .center_x(Length::Fill)
                .center_y(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_| container::Style {
            background: Some(backdrop_color.into()),
            ..Default::default()
        })
        .into()
    }

    fn view_workspace_bar(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let mut bar_row = Row::new().spacing(0).align_y(iced::Alignment::Center);

        let pulse_bright = self.attention_pulse_bright;

        for (idx, ws) in self.workspaces.iter().enumerate() {
            let is_active = idx == self.active_workspace_idx;
            let ws_color = ws.color.color(theme);
            let text_color = if is_active {
                ws_color
            } else {
                theme.overlay0()
            };
            let active_bg = theme.bg_base();
            let hover_bg = theme.surface0();

            let attn_count = ws.attention_count();
            let has_attention = attn_count > 0;
            let has_error = ws.console.status == ConsoleStatus::Error;

            // Colored dot before name — override for attention/error
            let dot_color = if has_error {
                theme.danger()
            } else if has_attention {
                if pulse_bright {
                    theme.peach()
                } else {
                    theme.warning()
                }
            } else if is_active {
                ws_color
            } else {
                theme.surface2()
            };
            let dot = container(iced::widget::Space::new().width(0).height(0))
                .width(Length::Fixed(6.0))
                .height(Length::Fixed(6.0))
                .style(move |_| container::Style {
                    background: Some(dot_color.into()),
                    border: iced::Border {
                        radius: 3.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                });

            let label = text(&ws.name)
                .size(11)
                .color(text_color)
                .font(iced::Font::with_name("Menlo"));

            let mut btn_content = row![dot, label].spacing(6).align_y(iced::Alignment::Center);

            // Gear icon button for active workspace settings
            if is_active {
                let gear_color = theme.overlay0();
                let gear_hover = theme.text_secondary();
                let gear_btn = button(
                    text("⚙")
                        .size(10)
                        .color(gear_color)
                        .font(iced::Font::with_name("Menlo")),
                )
                .style(move |_theme, status| button::Style {
                    background: Some(iced::Color::TRANSPARENT.into()),
                    text_color: if matches!(status, button::Status::Hovered) {
                        gear_hover
                    } else {
                        gear_color
                    },
                    border: iced::Border::default(),
                    ..Default::default()
                })
                .padding([0, 4])
                .on_press(Event::WorkspaceSettingsOpen(idx));
                btn_content = btn_content.push(gear_btn);
            }

            // Attention/error badge
            if has_error {
                let badge_bg = theme.danger();
                let badge_text_color = theme.bg_crust();
                btn_content = btn_content.push(
                    container(
                        text("!")
                            .size(9)
                            .color(badge_text_color)
                            .font(iced::Font::with_name("Menlo")),
                    )
                    .padding([0, 4])
                    .style(move |_| container::Style {
                        background: Some(badge_bg.into()),
                        border: iced::Border {
                            radius: 6.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                );
            } else if has_attention {
                let badge_bg = theme.peach();
                let badge_text_color = theme.bg_crust();
                btn_content = btn_content.push(
                    container(
                        text(format!("{}", attn_count))
                            .size(9)
                            .color(badge_text_color)
                            .font(iced::Font::with_name("Menlo")),
                    )
                    .padding([0, 4])
                    .style(move |_| container::Style {
                        background: Some(badge_bg.into()),
                        border: iced::Border {
                            radius: 6.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                );
            }

            // Active workspace: colored top accent line above the button
            if is_active {
                let accent_line = container(iced::widget::Space::new().width(0).height(0))
                    .width(Length::Fill)
                    .height(Length::Fixed(2.0))
                    .style(move |_| container::Style {
                        background: Some(ws_color.into()),
                        ..Default::default()
                    });

                let ws_btn = button(btn_content)
                    .style(move |_theme, _status| button::Style {
                        background: Some(active_bg.into()),
                        text_color: iced::Color::WHITE,
                        border: iced::Border::default(),
                        ..Default::default()
                    })
                    .padding([4, 12])
                    .on_press(Event::WorkspaceSelect(idx));

                let stacked = column![accent_line, ws_btn].spacing(0);
                bar_row = bar_row.push(stacked);
            } else {
                let ws_btn = button(btn_content)
                    .style(move |_theme, status| {
                        let bg = if matches!(status, button::Status::Hovered) {
                            hover_bg
                        } else {
                            iced::Color::TRANSPARENT
                        };
                        button::Style {
                            background: Some(bg.into()),
                            text_color: iced::Color::WHITE,
                            border: iced::Border::default(),
                            ..Default::default()
                        }
                    })
                    .padding([6, 12])
                    .on_press(Event::WorkspaceSelect(idx));

                bar_row = bar_row.push(ws_btn);
            }

            // Separator between workspaces
            if idx < self.workspaces.len() - 1 {
                let sep_color = theme.surface0();
                bar_row = bar_row.push(
                    container(iced::widget::Space::new().width(0).height(0))
                        .width(Length::Fixed(1.0))
                        .height(Length::Fixed(14.0))
                        .style(move |_| container::Style {
                            background: Some(sep_color.into()),
                            ..Default::default()
                        }),
                );
            }
        }

        // "+ workspace" button at the end
        let ws_add_color = theme.overlay0();
        let ws_add_hover = theme.overlay1();
        let ws_add_btn = button(
            text("+ workspace")
                .size(11)
                .color(ws_add_color)
                .font(iced::Font::with_name("Menlo")),
        )
        .style(move |_theme, status| {
            let tc = if matches!(status, button::Status::Hovered) {
                ws_add_hover
            } else {
                ws_add_color
            };
            button::Style {
                background: Some(iced::Color::TRANSPARENT.into()),
                text_color: tc,
                ..Default::default()
            }
        })
        .padding([6, 12])
        .on_press(Event::WorkspaceCreate);
        bar_row = bar_row.push(ws_add_btn);

        let scrollable_bar = scrollable(bar_row.padding([0, 4]).align_y(iced::Alignment::Center))
            .direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::new().width(0).scroller_width(0),
            ))
            .id(workspace_bar_scrollable_id())
            .width(Length::Fill)
            .style(|_theme, _status| {
                let transparent_rail = scrollable::Rail {
                    background: None,
                    border: iced::Border::default(),
                    scroller: scrollable::Scroller {
                        background: iced::Color::TRANSPARENT.into(),
                        border: iced::Border::default(),
                    },
                };
                scrollable::Style {
                    container: container::Style::default(),
                    vertical_rail: transparent_rail,
                    horizontal_rail: transparent_rail,
                    gap: None,
                    auto_scroll: scrollable::AutoScroll {
                        background: iced::Color::TRANSPARENT.into(),
                        border: iced::Border::default(),
                        shadow: iced::Shadow::default(),
                        icon: iced::Color::TRANSPARENT,
                    },
                }
            });

        let bg = theme.bg_crust();
        let top_border_color = theme.surface0();

        let top_border = container(iced::widget::Space::new().height(0))
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(move |_| container::Style {
                background: Some(top_border_color.into()),
                ..Default::default()
            });

        // Help button (?) pinned to the right
        let help_color = theme.overlay0();
        let help_hover = theme.overlay1();
        let help_btn = button(
            text("?")
                .size(11)
                .color(help_color)
                .font(iced::Font::with_name("Menlo")),
        )
        .style(move |_theme, status| {
            let tc = if matches!(status, button::Status::Hovered) {
                help_hover
            } else {
                help_color
            };
            button::Style {
                background: Some(iced::Color::TRANSPARENT.into()),
                text_color: tc,
                ..Default::default()
            }
        })
        .padding([6, 10])
        .on_press(Event::ToggleHelp);

        let bar_inner = row![scrollable_bar, help_btn]
            .spacing(0)
            .align_y(iced::Alignment::Center)
            .width(Length::Fill);

        let bar_container =
            container(bar_inner)
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(bg.into()),
                    ..Default::default()
                });

        column![top_border, bar_container].into()
    }

    fn view_spine(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let pulse_bright = self.attention_pulse_bright;
        let mut dots = Column::new().spacing(8).align_x(iced::Alignment::Center);

        for (idx, ws) in self.workspaces.iter().enumerate() {
            let is_active = idx == self.active_workspace_idx;
            let ws_color = ws.color.color(theme);
            let inactive_color = theme.surface2();

            let has_attention = ws.has_attention();
            let has_error = ws.console.status == ConsoleStatus::Error;

            // Larger dot for attention/error when inactive
            let (dot_w, dot_h) = if is_active {
                (4.0, 18.0)
            } else if has_attention || has_error {
                (6.0, 6.0)
            } else {
                (4.0, 4.0)
            };

            // Color: error (red) > attention (pulsing amber) > active (ws color) > inactive
            let dot_color = if has_error && !is_active {
                theme.danger()
            } else if has_attention && !is_active {
                if pulse_bright {
                    theme.peach()
                } else {
                    theme.warning()
                }
            } else if is_active {
                ws_color
            } else {
                inactive_color
            };

            let dot = container(iced::widget::Space::new().width(0).height(0))
                .width(Length::Fixed(dot_w))
                .height(Length::Fixed(dot_h))
                .style(move |_| container::Style {
                    background: Some(dot_color.into()),
                    border: iced::Border {
                        radius: 2.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                });

            let hover_bg = theme.surface0();
            let dot_btn = button(
                container(dot)
                    .width(Length::Fixed(SPINE_WIDTH - 1.0))
                    .center_x(Length::Fixed(SPINE_WIDTH - 1.0))
                    .center_y(Length::Shrink),
            )
            .style(move |_theme, status| {
                let bg = if matches!(status, button::Status::Hovered) {
                    hover_bg
                } else {
                    iced::Color::TRANSPARENT
                };
                button::Style {
                    background: Some(bg.into()),
                    border: iced::Border::default(),
                    text_color: iced::Color::WHITE,
                    ..Default::default()
                }
            })
            .padding([4, 0])
            .on_press(Event::WorkspaceSelect(idx));

            dots = dots.push(dot_btn);
        }

        let bg = theme.bg_crust();
        let border_color = theme.surface0();

        let spine_content = container(container(dots).height(Length::Fill).center_y(Length::Fill))
            .width(Length::Fixed(SPINE_WIDTH))
            .height(Length::Fill)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                ..Default::default()
            });

        // Right border as a separate 1px column
        let border_line = container(iced::widget::Space::new().width(0).height(0))
            .width(Length::Fixed(1.0))
            .height(Length::Fill)
            .style(move |_| container::Style {
                background: Some(border_color.into()),
                ..Default::default()
            });

        row![spine_content, border_line].into()
    }

    fn view_tab_bar(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;

        // === Left section: scrollable tabs ===
        let mut tabs_row = Row::new().spacing(2);

        // Left edge peek indicator
        if self.edge_peek_left {
            if let Some(left_ws) = self
                .workspaces
                .get(self.active_workspace_idx.wrapping_sub(1))
            {
                let ws_color = left_ws.color.color(theme);
                let peek_btn = button(
                    text(format!("\u{2039} {}", left_ws.name))
                        .size(11)
                        .color(ws_color)
                        .font(iced::Font::with_name("Menlo")),
                )
                .style(move |_theme, _status| button::Style {
                    background: Some(iced::Color::TRANSPARENT.into()),
                    text_color: ws_color,
                    ..Default::default()
                })
                .padding([4, 6])
                .on_press(Event::WorkspaceSelect(self.active_workspace_idx - 1));
                tabs_row = tabs_row.push(peek_btn);
            }
        }

        let (tabs, active_tab_idx) = if let Some(ws) = self.active_workspace() {
            (ws.tabs.as_slice(), ws.active_tab)
        } else {
            (&[] as &[TabState], 0)
        };

        let pulse_bright = self.attention_pulse_bright;

        for (idx, tab) in tabs.iter().enumerate() {
            let is_active = idx == active_tab_idx;
            let has_attention = tab.needs_attention;

            // Determine if this is a Claude Code tab
            let is_claude = tab
                .terminal_title()
                .map(|t| t.to_lowercase().contains("claude"))
                .unwrap_or(false);

            // Icon prefix — attention overrides normal icon
            let (icon_str, icon_color) = if has_attention {
                let attn_color = if pulse_bright {
                    theme.peach()
                } else {
                    theme.warning()
                };
                ("● ", attn_color)
            } else if is_claude {
                ("✦ ", theme.peach())
            } else {
                ("▶ ", theme.success())
            };

            // Tab label - strip leading "*" when attention (redundant with visual indicator),
            // shorten path-like titles to last component, truncate at 20 chars
            let base_title = tab
                .terminal_title()
                .map(|t| {
                    let display = if has_attention {
                        t.trim_start_matches('*').trim_start()
                    } else {
                        t
                    };
                    // Path-like titles (e.g. from Codex) — extract last component
                    let display = if display.starts_with('/') || display.starts_with('~') {
                        std::path::Path::new(display)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| display.to_string())
                    } else {
                        display.to_string()
                    };
                    if display.len() > 20 {
                        format!("{}…", truncate_str(&display, 19))
                    } else {
                        display
                    }
                })
                .unwrap_or_else(|| tab.repo_name.clone());

            let text_color = if is_active {
                theme.text_primary()
            } else {
                theme.overlay1()
            };
            let active_bg = theme.bg_base();
            let hover_bg = theme.surface0();

            // Attention background colors
            let attn_bg_color = if pulse_bright {
                iced::Color {
                    a: 0.20,
                    ..theme.peach()
                }
            } else {
                iced::Color {
                    a: 0.12,
                    ..theme.peach()
                }
            };
            let attn_border_color = iced::Color {
                a: 0.5,
                ..theme.peach()
            };

            // Build tab content: icon + label + shortcut
            let mut tab_content = Row::new().spacing(0).align_y(iced::Alignment::Center);
            tab_content = tab_content.push(text(icon_str).size(12).color(icon_color));
            tab_content = tab_content.push(
                text(base_title)
                    .size(13)
                    .color(text_color)
                    .font(iced::Font::with_name("Menlo")),
            );

            if idx < 9 {
                tab_content = tab_content.push(
                    text(format!(" \u{2318}{}", idx + 1))
                        .size(10)
                        .color(theme.surface2())
                        .font(iced::Font::with_name("Menlo")),
                );
            }

            let tab_btn = button(tab_content)
                .style(move |_theme, status| {
                    if has_attention {
                        // Attention style takes priority
                        button::Style {
                            background: Some(attn_bg_color.into()),
                            border: iced::Border {
                                radius: 6.0.into(),
                                color: attn_border_color,
                                width: 1.0,
                            },
                            text_color: iced::Color::WHITE,
                            ..Default::default()
                        }
                    } else {
                        let bg = if is_active {
                            Some(active_bg.into())
                        } else if matches!(status, button::Status::Hovered) {
                            Some(hover_bg.into())
                        } else {
                            Some(iced::Color::TRANSPARENT.into())
                        };
                        button::Style {
                            background: bg,
                            border: iced::Border {
                                radius: 6.0.into(),
                                ..Default::default()
                            },
                            text_color: iced::Color::WHITE,
                            ..Default::default()
                        }
                    }
                })
                .padding([4, 10])
                .on_press(Event::TabSelect(idx));

            // Close button
            let close_color = theme.overlay0();
            let close_hover = theme.text_primary();
            let close_btn = button(text("\u{00d7}").size(14).color(close_color))
                .style(move |_theme, status| {
                    let tc = if matches!(status, button::Status::Hovered) {
                        close_hover
                    } else {
                        close_color
                    };
                    button::Style {
                        background: Some(iced::Color::TRANSPARENT.into()),
                        text_color: tc,
                        ..Default::default()
                    }
                })
                .padding([4, 4])
                .on_press(Event::TabClose(idx));

            tabs_row = tabs_row.push(
                row![tab_btn, close_btn]
                    .spacing(0)
                    .align_y(iced::Alignment::Center),
            );
        }

        // Add tab button
        let add_color = theme.overlay0();
        let add_hover = theme.text_primary();
        let add_btn = button(text("+").size(14).color(add_color))
            .style(move |_theme, status| {
                let tc = if matches!(status, button::Status::Hovered) {
                    add_hover
                } else {
                    add_color
                };
                button::Style {
                    background: Some(iced::Color::TRANSPARENT.into()),
                    text_color: tc,
                    ..Default::default()
                }
            })
            .padding([4, 8])
            .on_press(if self.agent_presets.is_empty() {
                Event::NewPlainTab
            } else {
                Event::LaunchAgentPreset(0)
            });
        tabs_row = tabs_row.push(add_btn);

        // Wrap tabs in a horizontal scrollable
        let scrollable_tabs = scrollable(tabs_row.padding([4, 8]).align_y(iced::Alignment::Center))
            .direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::new().width(0).scroller_width(0),
            ))
            .id(tab_scrollable_id())
            .width(Length::Fill)
            .style(|_theme, _status| {
                let transparent_rail = scrollable::Rail {
                    background: None,
                    border: iced::Border::default(),
                    scroller: scrollable::Scroller {
                        background: iced::Color::TRANSPARENT.into(),
                        border: iced::Border::default(),
                    },
                };
                scrollable::Style {
                    container: container::Style::default(),
                    vertical_rail: transparent_rail,
                    horizontal_rail: transparent_rail,
                    gap: None,
                    auto_scroll: scrollable::AutoScroll {
                        background: iced::Color::TRANSPARENT.into(),
                        border: iced::Border::default(),
                        shadow: iced::Shadow::default(),
                        icon: iced::Color::TRANSPARENT,
                    },
                }
            });

        // === Right section: fixed workspace metadata ===
        let mut metadata_row = Row::new().spacing(4);

        // STT mic indicator
        #[cfg(feature = "stt")]
        if self.stt_enabled {
            let (mic_icon, mic_color) = if self.stt_recording {
                // Pulsing red/peach mic when recording
                let c = if self.attention_pulse_bright {
                    theme.danger()
                } else {
                    theme.peach()
                };
                ("\u{25CF} REC", c) // ● REC
            } else if self.stt_transcribing {
                ("\u{2026}", theme.warning()) // … (processing)
            } else {
                ("\u{25CB}", theme.overlay0()) // ○ grey idle
            };
            metadata_row = metadata_row.push(
                text(mic_icon)
                    .size(11)
                    .color(mic_color)
                    .font(iced::Font::with_name("Menlo")),
            );
        }

        if let Some(ws) = self.active_workspace() {
            let ws_color = ws.color.color(theme);
            metadata_row = metadata_row.push(
                text(&ws.name)
                    .size(12)
                    .color(ws_color)
                    .font(iced::Font::with_name("Menlo")),
            );

            // Close workspace button (only if more than one workspace)
            if self.workspaces.len() > 1 {
                let close_color = theme.overlay0();
                let close_hover = theme.text_primary();
                let ws_idx = self.active_workspace_idx;
                let close_ws_btn = button(text("\u{00d7}").size(12).color(close_color))
                    .style(move |_theme, status| {
                        let tc = if matches!(status, button::Status::Hovered) {
                            close_hover
                        } else {
                            close_color
                        };
                        button::Style {
                            background: Some(iced::Color::TRANSPARENT.into()),
                            text_color: tc,
                            ..Default::default()
                        }
                    })
                    .padding([2, 4])
                    .on_press(Event::WorkspaceClose(ws_idx));
                metadata_row = metadata_row.push(close_ws_btn);
            }

            if let Some(tab) = self.active_tab() {
                metadata_row = metadata_row.push(
                    text(format!(" \u{e0a0} {}", tab.branch_name))
                        .size(12)
                        .color(theme.overlay0())
                        .font(iced::Font::with_name("Menlo")),
                );
            }
        }

        // Right edge peek indicator
        if self.edge_peek_right {
            if let Some(right_ws) = self.workspaces.get(self.active_workspace_idx + 1) {
                let ws_color = right_ws.color.color(theme);
                let peek_btn = button(
                    text(format!("{} \u{203a}", right_ws.name))
                        .size(11)
                        .color(ws_color)
                        .font(iced::Font::with_name("Menlo")),
                )
                .style(move |_theme, _status| button::Style {
                    background: Some(iced::Color::TRANSPARENT.into()),
                    text_color: ws_color,
                    ..Default::default()
                })
                .padding([4, 6])
                .on_press(Event::WorkspaceSelect(self.active_workspace_idx + 1));
                metadata_row = metadata_row.push(peek_btn);
            }
        }

        // === Combine: scrollable tabs (fill) + fixed metadata (shrink) ===
        let bg = theme.bg_crust();
        let border_color = theme.surface0();

        let combined_row = Row::new()
            .push(scrollable_tabs)
            .push(
                metadata_row
                    .padding([4, 8])
                    .align_y(iced::Alignment::Center),
            )
            .align_y(iced::Alignment::Center);

        let tab_container =
            container(combined_row)
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(bg.into()),
                    ..Default::default()
                });

        // Bottom border as separator
        let separator = container(iced::widget::Space::new().height(0))
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(move |_| container::Style {
                background: Some(border_color.into()),
                ..Default::default()
            });

        column![tab_container, separator].into()
    }

    fn view_workspace_content<'a>(
        &'a self,
        ws: &'a Workspace,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        if let Some(tab) = ws.active_tab() {
            let main_panel = if matches!(tab.kind, TabKind::Agent(_)) {
                // Agent tabs render the chat UI in the wry webview which is
                // positioned by `calculate_webview_bounds` and managed via
                // `show_agent_webview`. The Iced view here is just an empty
                // background that lets the webview occupy the same area.
                freeze_time!("view_agent_tab", { self.view_agent_tab(tab) })
            } else if tab.agent_sidebar.selected_capture_idx.is_some()
                && tab.sidebar_mode == SidebarMode::Agent
            {
                freeze_time!("view_agent_conversation", {
                    self.view_agent_conversation(tab)
                })
            } else if tab.viewing_file_path().is_some() {
                freeze_time!("view_file_content", { self.view_file_content(tab) })
            } else if tab.selected_file.is_some() {
                freeze_time!("view_diff_panel", { self.view_diff_panel(tab) })
            } else {
                freeze_time!("view_terminal", { self.view_terminal(tab) })
            };

            if self.sidebar_collapsed {
                let icon_rail = self.view_sidebar_rail(tab);
                let border_color = theme.border();
                let rail_border = container(iced::widget::Space::new())
                    .width(Length::Fixed(1.0))
                    .height(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(border_color.into()),
                        ..Default::default()
                    });

                row![icon_rail, rail_border, main_panel]
                    .spacing(0)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into()
            } else {
                let sidebar = freeze_time!("view_sidebar", { self.view_sidebar(tab) });

                // Draggable divider
                let divider_color = if self.dragging_divider {
                    theme.accent()
                } else {
                    theme.border()
                };
                let divider = iced::widget::mouse_area(
                    container(iced::widget::Space::new())
                        .width(Length::Fixed(4.0))
                        .height(Length::Fill)
                        .style(move |_| container::Style {
                            background: Some(divider_color.into()),
                            ..Default::default()
                        }),
                )
                .on_press(Event::DividerDragStart)
                .interaction(iced::mouse::Interaction::ResizingHorizontally);

                row![sidebar, divider, main_panel]
                    .spacing(0)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into()
            }
        } else {
            let bg = theme.bg_base();
            container(
                column![
                    text("No repository open")
                        .size(16)
                        .color(theme.text_primary()),
                    button(text("Open Folder").size(14))
                        .style(button::primary)
                        .padding([8, 16])
                        .on_press(Event::OpenFolder)
                ]
                .spacing(16)
                .align_x(iced::Alignment::Center),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                ..Default::default()
            })
            .into()
        }
    }

    fn view_workspace_slide(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let viewport_width = self.content_viewport_width();
        let active_idx = self.active_workspace_idx;
        let theme = &self.theme;

        // When not animating or swiping, render active workspace directly
        // (avoids scroll_to timing issues, especially after creating new workspaces)
        let is_swiping = self.last_user_scroll.is_some();
        if !self.slide_animating && !is_swiping {
            if let Some(ws) = self.workspaces.get(active_idx) {
                return container(self.view_workspace_content(ws))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .clip(true)
                    .into();
            }
        }

        let mut panels = Row::new().spacing(0);

        for (idx, ws) in self.workspaces.iter().enumerate() {
            // Only render full content for active workspace and immediate neighbors
            let panel: Element<'_, Event, Theme, iced::Renderer> =
                if (idx as i32 - active_idx as i32).unsigned_abs() <= 1 {
                    self.view_workspace_content(ws)
                } else {
                    // Distant workspace: render colored placeholder
                    let bg = theme.bg_base();
                    container(iced::widget::Space::new())
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .style(move |_| container::Style {
                            background: Some(bg.into()),
                            ..Default::default()
                        })
                        .into()
                };

            let panel_container = container(panel)
                .width(Length::Fixed(viewport_width))
                .height(Length::Fill)
                .clip(true);
            panels = panels.push(panel_container);
        }

        scrollable(panels)
            .direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::new().width(0).scroller_width(0),
            ))
            .id(workspace_scrollable_id())
            .on_scroll(Event::SlideScrolled)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_theme, _status| {
                let transparent_rail = scrollable::Rail {
                    background: None,
                    border: iced::Border::default(),
                    scroller: scrollable::Scroller {
                        background: iced::Color::TRANSPARENT.into(),
                        border: iced::Border::default(),
                    },
                };
                scrollable::Style {
                    container: container::Style::default(),
                    vertical_rail: transparent_rail,
                    horizontal_rail: transparent_rail,
                    gap: None,
                    auto_scroll: scrollable::AutoScroll {
                        background: iced::Color::TRANSPARENT.into(),
                        border: iced::Border::default(),
                        shadow: iced::Shadow::default(),
                        icon: iced::Color::TRANSPARENT,
                    },
                }
            })
            .into()
    }

    fn view_sidebar<'a>(&'a self, tab: &'a TabState) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let mut content = Column::new().spacing(0);

        // Mode toggle buttons
        let toggle = self.view_sidebar_toggle(tab);
        content = content.push(toggle);

        // Content based on mode
        let mode_content: Element<'_, Event, Theme, iced::Renderer> = match tab.sidebar_mode {
            SidebarMode::Git => freeze_time!("view_git_list", { self.view_git_list(tab) }),
            SidebarMode::Files => freeze_time!("view_file_tree", { self.view_file_tree(tab) }),
            SidebarMode::Claude => {
                freeze_time!("view_claude_sidebar", { self.view_claude_sidebar(tab) })
            }
            SidebarMode::Agent => {
                freeze_time!("view_agent_sidebar", { self.view_agent_sidebar(tab) })
            }
            SidebarMode::Plans => {
                freeze_time!("view_plans_sidebar", { self.view_plans_sidebar() })
            }
        };

        content = content.push(mode_content);

        let bg = theme.bg_surface();
        container(content)
            .width(Length::Fixed(self.sidebar_width))
            .height(Length::Fill)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                ..Default::default()
            })
            .into()
    }

    /// Collapsed sidebar: vertical icon rail with single-letter mode buttons
    fn view_sidebar_rail<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();

        let rail_width: f32 = 36.0;

        let modes = [
            ("\u{2387}", SidebarMode::Git),    // ⎇ branch symbol
            ("\u{1F4C1}", SidebarMode::Files), // 📁 folder
            ("\u{2726}", SidebarMode::Claude), // ✦ sparkle
        ];

        let mut rail_col = Column::new().spacing(0).width(Length::Fixed(rail_width));

        for (label, mode) in &modes {
            let is_active = tab.sidebar_mode == *mode;
            let text_color = if is_active {
                theme.text_primary()
            } else {
                theme.overlay1()
            };
            let accent = theme.accent();
            let hover_bg = theme.surface0();

            // Active indicator: accent bar on the left edge
            let indicator_color = if is_active {
                accent
            } else {
                iced::Color::TRANSPARENT
            };
            let indicator = container(iced::widget::Space::new())
                .width(Length::Fixed(2.0))
                .height(Length::Fixed(24.0))
                .style(move |_| container::Style {
                    background: Some(indicator_color.into()),
                    border: iced::Border {
                        radius: 1.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                });

            let letter = text(*label).size(font).color(text_color);

            let btn_content = row![
                indicator,
                container(letter)
                    .center_x(Length::Fill)
                    .center_y(Length::Fixed(24.0))
            ]
            .spacing(0)
            .align_y(iced::Alignment::Center)
            .width(Length::Fixed(rail_width));

            let mode_clone = *mode;
            let mode_btn = button(btn_content)
                .style(move |_theme, status| {
                    let bg = if matches!(status, button::Status::Hovered) {
                        Some(hover_bg.into())
                    } else {
                        Some(iced::Color::TRANSPARENT.into())
                    };
                    button::Style {
                        background: bg,
                        border: iced::Border::default(),
                        text_color,
                        ..Default::default()
                    }
                })
                .padding([8, 0])
                .width(Length::Fixed(rail_width))
                .on_press(Event::SetSidebarMode(mode_clone));

            rail_col = rail_col.push(mode_btn);
        }

        // Spacer to push expand chevron to bottom
        rail_col = rail_col.push(iced::widget::Space::new().height(Length::Fill));

        // Expand chevron at bottom
        let chevron_color = theme.overlay0();
        let hover_bg = theme.surface0();
        let expand_btn = button(
            container(text("\u{25B6}").size(10).color(chevron_color))
                .center_x(Length::Fixed(rail_width)),
        )
        .style(move |_theme, status| {
            let bg = if matches!(status, button::Status::Hovered) {
                Some(hover_bg.into())
            } else {
                Some(iced::Color::TRANSPARENT.into())
            };
            button::Style {
                background: bg,
                border: iced::Border::default(),
                ..Default::default()
            }
        })
        .padding([8, 0])
        .width(Length::Fixed(rail_width))
        .on_press(Event::ToggleSidebar);

        rail_col = rail_col.push(expand_btn);

        let bg = theme.bg_surface();
        container(rail_col)
            .width(Length::Fixed(rail_width))
            .height(Length::Fill)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                ..Default::default()
            })
            .into()
    }

    fn view_sidebar_tab<'a>(
        &'a self,
        label: Element<'a, Event, Theme, iced::Renderer>,
        is_active: bool,
        on_press: Event,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let text_color = if is_active {
            theme.text_primary()
        } else {
            theme.overlay1()
        };
        let underline_color = if is_active {
            theme.accent()
        } else {
            iced::Color::TRANSPARENT
        };
        let hover_bg = theme.surface0();

        let tab_btn = button(label)
            .style(move |_theme, status| {
                let bg = if matches!(status, button::Status::Hovered) {
                    Some(hover_bg.into())
                } else {
                    Some(iced::Color::TRANSPARENT.into())
                };
                button::Style {
                    background: bg,
                    border: iced::Border::default(),
                    text_color,
                    ..Default::default()
                }
            })
            .padding([4, 10])
            .width(Length::Fill)
            .on_press(on_press);

        let underline = container(iced::widget::Space::new())
            .width(Length::Fill)
            .height(Length::Fixed(2.0))
            .style(move |_| container::Style {
                background: Some(underline_color.into()),
                ..Default::default()
            });

        column![tab_btn, underline]
            .spacing(0)
            .width(Length::FillPortion(1))
            .into()
    }

    fn view_sidebar_toggle<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let changes = tab.total_changes();

        let git_active = tab.sidebar_mode == SidebarMode::Git;
        let files_active = tab.sidebar_mode == SidebarMode::Files;
        let claude_active = tab.sidebar_mode == SidebarMode::Claude;
        let _agent_active = tab.sidebar_mode == SidebarMode::Agent;

        // Git tab label with optional badge
        let git_text_color = if git_active {
            theme.text_primary()
        } else {
            theme.overlay1()
        };
        let mut git_label: Row<'_, Event, Theme, iced::Renderer> =
            Row::new().spacing(4).align_y(iced::Alignment::Center);
        git_label = git_label.push(text("Git").size(font).color(git_text_color));

        if changes > 0 {
            let badge_bg = iced::Color {
                a: 0.2,
                ..theme.warning()
            };
            let warning_color = theme.warning();
            let badge = container(text(format!("{}", changes)).size(10).color(warning_color))
                .padding([1, 6])
                .style(move |_| container::Style {
                    background: Some(badge_bg.into()),
                    border: iced::Border {
                        radius: 8.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                });
            git_label = git_label.push(badge);
        }

        let git_tab = self.view_sidebar_tab(
            git_label.into(),
            git_active,
            Event::SetSidebarMode(SidebarMode::Git),
        );

        // Files tab
        let files_text_color = if files_active {
            theme.text_primary()
        } else {
            theme.overlay1()
        };
        let files_tab = self.view_sidebar_tab(
            text("Files").size(font).color(files_text_color).into(),
            files_active,
            Event::SetSidebarMode(SidebarMode::Files),
        );

        // Agent tab (was "Claude" — same SidebarMode::Claude variant, shows skills/plugins/hooks)
        let agent_text_color = if claude_active {
            theme.text_primary()
        } else {
            theme.overlay1()
        };
        let agent_tab = self.view_sidebar_tab(
            text("Agent").size(font).color(agent_text_color).into(),
            claude_active,
            Event::SetSidebarMode(SidebarMode::Claude),
        );

        let bg = theme.bg_crust();
        let border_color = theme.surface0();

        // Collapse chevron (same style as console toggle)
        let chevron_color = theme.overlay0();
        let collapse_chevron = button(
            text("\u{25C0}").size(10).color(chevron_color), // ◀ left-pointing
        )
        .style(|_theme, _status| button::Style {
            background: Some(iced::Color::TRANSPARENT.into()),
            ..Default::default()
        })
        .padding([4, 4])
        .on_press(Event::ToggleSidebar);

        // Plans tab (replaces the legacy "Agent" sidebar tab)
        let plans_active = tab.sidebar_mode == SidebarMode::Plans;
        let plans_text_color = if plans_active {
            theme.text_primary()
        } else {
            theme.overlay1()
        };
        let plans_tab = self.view_sidebar_tab(
            text("Plans").size(font).color(plans_text_color).into(),
            plans_active,
            Event::SetSidebarMode(SidebarMode::Plans),
        );

        let tab_row =
            container(row![git_tab, files_tab, agent_tab, plans_tab, collapse_chevron].spacing(0))
                .padding([4, 4])
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(bg.into()),
                    ..Default::default()
                });

        let separator = container(iced::widget::Space::new().height(0))
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(move |_| container::Style {
                background: Some(border_color.into()),
                ..Default::default()
            });

        column![tab_row, separator].into()
    }

    fn view_file_tree<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let font_small = self.ui_font_small();

        // Debug large file trees
        if tab.file_tree.len() > 500 {
            freeze_debug!(
                "Large file tree with {} entries in view_file_tree",
                tab.file_tree.len()
            );
        }

        let mut content = Column::new().spacing(2).padding(8);

        // Current path - show relative to repo if inside it, otherwise show with ~ for home
        let home = std::env::var("HOME").unwrap_or_default();
        let path_display = if let Ok(rel_path) = tab.current_dir.strip_prefix(&tab.repo_path) {
            // Inside repo - show repo_name/relative/path/
            if rel_path.as_os_str().is_empty() {
                format!("{}/", tab.repo_name)
            } else {
                format!("{}/{}/", tab.repo_name, rel_path.display())
            }
        } else if let Ok(rel_home) = tab.current_dir.strip_prefix(&home) {
            // Outside repo but under home - show ~/path/
            format!("~/{}/", rel_home.display())
        } else {
            // Absolute path
            format!("{}/", tab.current_dir.display())
        };

        // Path display
        content = content.push(
            row![text(path_display).size(font).color(theme.accent()),]
                .padding([4, 0])
                .align_y(iced::Alignment::Center),
        );

        // Up button (if not at repo root)
        if tab.current_dir != tab.repo_path {
            let muted = theme.text_secondary();
            content = content.push(
                button(
                    row![
                        text("..")
                            .size(font)
                            .color(muted)
                            .width(Length::Fixed(20.0)),
                        text("(parent)").size(font_small).color(muted),
                    ]
                    .spacing(8),
                )
                .style(button::text)
                .padding([4, 8])
                .on_press(Event::NavigateUp),
            );
        }

        // File tree entries
        for entry in &tab.file_tree {
            let is_selected_file = !entry.is_dir
                && tab
                    .viewing_file_path()
                    .is_some_and(|selected| selected == entry.path.as_path());
            let (icon, name_suffix, icon_color, name_color, bg_color) = if entry.is_dir {
                // Folders: blue folder icon, trailing /, light background
                (
                    "📁",
                    "/",
                    theme.accent(),
                    theme.accent(),
                    Some(theme.bg_base()),
                )
            } else {
                // Files: colored by extension
                let ext = entry
                    .path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("");
                let file_color = match ext {
                    "ts" | "tsx" => theme.accent(),
                    "js" | "jsx" => theme.warning(),
                    "md" => theme.success(),
                    "json" => theme.warning(),
                    "rs" => match self.theme {
                        AppTheme::Dark => color!(0xfab387),
                        AppTheme::Light => color!(0xfe640b),
                    },
                    "toml" | "yml" | "yaml" => match self.theme {
                        AppTheme::Dark => color!(0x94e2d5),
                        AppTheme::Light => color!(0x179299),
                    },
                    "css" | "scss" => match self.theme {
                        AppTheme::Dark => color!(0xcba6f7),
                        AppTheme::Light => color!(0x8839ef),
                    },
                    "html" => theme.danger(),
                    _ => theme.text_secondary(),
                };
                let file_name_color = if is_selected_file {
                    match self.theme {
                        AppTheme::Dark => color!(0xffffff),
                        AppTheme::Light => color!(0xffffff),
                    }
                } else {
                    theme.text_primary()
                };
                ("  ", "", file_color, file_name_color, None)
            };

            let entry_row = row![
                text(icon)
                    .size(font)
                    .color(icon_color)
                    .width(Length::Fixed(24.0)),
                text(format!("{}{}", entry.name, name_suffix))
                    .size(font)
                    .color(name_color),
            ]
            .spacing(4);

            let event = if entry.is_dir {
                Event::NavigateDir(entry.path.clone())
            } else {
                Event::ViewFile(entry.path.clone())
            };

            let row_btn_style = if is_selected_file {
                button::primary
            } else {
                button::text
            };

            let file_btn = button(entry_row)
                .style(row_btn_style)
                .padding([4, 8])
                .width(Length::Fill)
                .on_press(event);

            // For files, add an edit button; for dirs, just use the nav button
            let btn: Element<'a, Event, Theme, iced::Renderer> = if !entry.is_dir {
                let edit_btn = button(
                    text("\u{270e}")
                        .size(font_small)
                        .color(theme.text_secondary()),
                )
                .style(button::text)
                .padding([4, 6])
                .on_press(Event::EditFile(entry.path.clone()));
                row![file_btn, edit_btn]
                    .align_y(iced::Alignment::Center)
                    .into()
            } else {
                file_btn.into()
            };

            if let Some(bg) = bg_color {
                content = content.push(container(btn).width(Length::Fill).style(move |_| {
                    container::Style {
                        background: Some(bg.into()),
                        ..Default::default()
                    }
                }));
            } else {
                content = content.push(btn);
            }
        }

        if tab.file_tree.is_empty() {
            content = content.push(
                text("Empty directory")
                    .size(font)
                    .color(theme.text_secondary()),
            );
        }

        scrollable(content)
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
    }

    /// Sidebar list of markdown files from the active workspace's `.plans/`
    /// and `docs/` directories. Reads fresh on render; cache if it grows.
    fn view_plans_sidebar(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let font_small = self.ui_font_small();

        let Some(workspace_dir) = self
            .workspaces
            .get(self.active_workspace_idx)
            .map(|ws| &ws.dir)
        else {
            return container(
                text("No active workspace")
                    .size(font_small)
                    .color(theme.text_secondary()),
            )
            .padding(16)
            .into();
        };

        fn is_safe_sidebar_document_path(path: &str) -> bool {
            !path.is_empty()
                && path.ends_with(".md")
                && !path.starts_with('/')
                && !path.starts_with('\\')
                && path.split('/').all(|segment| {
                    !segment.is_empty()
                        && segment != "."
                        && segment != ".."
                        && segment != "_archive"
                        && !segment.starts_with('.')
                        && !segment.contains('\\')
                })
        }

        fn extract_sidebar_document_title(content: &str, fallback: &str) -> String {
            content
                .lines()
                .find_map(|l| {
                    l.trim_start()
                        .strip_prefix("# ")
                        .map(|s| s.trim().to_string())
                })
                .unwrap_or_else(|| fallback.trim_end_matches(".md").to_string())
        }

        fn collect_sidebar_document_entries(
            root: &Path,
            dir: &Path,
            entries: &mut Vec<(String, String)>,
        ) {
            let Ok(read_dir) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in read_dir.flatten() {
                let path = entry.path();
                let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if file_name.starts_with('.') || file_name == "_archive" {
                    continue;
                }
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() {
                    collect_sidebar_document_entries(root, &path, entries);
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                let Ok(rel_path) = path.strip_prefix(root) else {
                    continue;
                };
                let name = rel_path.to_string_lossy().replace('\\', "/");
                if !is_safe_sidebar_document_path(&name) {
                    continue;
                }
                let content = std::fs::read_to_string(&path).unwrap_or_default();
                let title = extract_sidebar_document_title(&content, &name);
                entries.push((name, title));
            }
        }

        let collect_entries = |dir: &Path| -> Vec<(String, String)> {
            let mut entries: Vec<(String, String)> = Vec::new();
            collect_sidebar_document_entries(dir, dir, &mut entries);
            entries.sort_by_key(|a| a.1.to_lowercase());
            entries
        };

        let mut list = Column::new().spacing(2).padding(8);
        for (source, label, dir) in [
            (
                plans_viewer::DocumentSource::Plans,
                "Plans",
                workspace_dir.join(".plans"),
            ),
            (
                plans_viewer::DocumentSource::Docs,
                "Docs",
                workspace_dir.join("docs"),
            ),
        ] {
            list = list.push(text(label.to_uppercase()).size(10).color(theme.overlay0()));

            let entries = collect_entries(&dir);
            if entries.is_empty() {
                list = list.push(
                    column![
                        text(format!("No {} found", label.to_lowercase()))
                            .size(font_small)
                            .color(theme.text_secondary()),
                        text(format!("Looked in {}", dir.display()))
                            .size(font_small)
                            .color(theme.text_muted()),
                    ]
                    .spacing(2)
                    .padding([2, 8]),
                );
            } else {
                for (name, title) in entries {
                    let button_content: Element<'_, Event, Theme, iced::Renderer> =
                        if name.contains('/') {
                            column![
                                text(title).size(font).color(theme.text_primary()),
                                text(name.clone())
                                    .size(font_small)
                                    .color(theme.text_muted()),
                            ]
                            .spacing(1)
                            .into()
                        } else {
                            text(title).size(font).color(theme.text_primary()).into()
                        };
                    let btn = button(button_content)
                        .style(self.ghost_button_style())
                        .padding([6, 8])
                        .width(Length::Fill)
                        .on_press(Event::OpenPlan(source, name));
                    list = list.push(btn);
                }
            }
        }

        iced::widget::scrollable(list)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn view_agent_sidebar<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let font_small = self.ui_font_small();

        if tab.agent_sidebar.loading {
            let spinner = container(
                text("Loading agent activity...")
                    .size(font_small)
                    .color(theme.subtext0()),
            )
            .padding(20)
            .center_x(Length::Fill);

            return spinner.into();
        }

        let activity = match &tab.agent_sidebar.activity {
            Some(activity) => activity,
            None => {
                let no_data = container(
                    column![
                        text("No Agent Activity")
                            .size(font)
                            .color(theme.text_primary()),
                        text("No captured sessions found for this repository")
                            .size(font_small)
                            .color(theme.subtext0())
                    ]
                    .spacing(4),
                )
                .padding(20)
                .center_x(Length::Fill);

                return no_data.into();
            }
        };

        let mut content = Column::new().spacing(0);

        // Header with overview stats
        let header_bg = theme.surface0();
        let accent_glow = iced::Color {
            a: 0.1,
            ..theme.mauve()
        };

        let commits_count = activity.total_commits;
        let live_count = activity.live_capture_count();
        let git_count = commits_count - live_count;
        let cost_display = activity.format_total_cost();

        let mut stats_row = Row::new().spacing(8);

        // Agent sessions badge (live captures)
        if live_count > 0 {
            stats_row = stats_row.push(
                container(
                    text(format!("{} sessions", live_count))
                        .size(font_small)
                        .color(theme.mauve()),
                )
                .padding([2, 8])
                .style(move |_| container::Style {
                    background: Some(
                        iced::Color {
                            a: 0.15,
                            ..theme.mauve()
                        }
                        .into(),
                    ),
                    border: iced::Border::default().rounded(8),
                    ..Default::default()
                }),
            );
        }

        // Git commits badge (reconstructed)
        if git_count > 0 {
            stats_row = stats_row.push(
                container(
                    text(format!("{} commits", git_count))
                        .size(font_small)
                        .color(theme.overlay1()),
                )
                .padding([2, 8])
                .style(move |_| container::Style {
                    background: Some(
                        iced::Color {
                            a: 0.12,
                            ..theme.overlay1()
                        }
                        .into(),
                    ),
                    border: iced::Border::default().rounded(8),
                    ..Default::default()
                }),
            );
        }

        // Cost badge (only if there are live captures with cost)
        if activity.total_cost > 0.0 {
            stats_row = stats_row.push(
                container(text(cost_display).size(font_small).color(theme.peach()))
                    .padding([2, 8])
                    .style(move |_| container::Style {
                        background: Some(
                            iced::Color {
                                a: 0.15,
                                ..theme.peach()
                            }
                            .into(),
                        ),
                        border: iced::Border::default().rounded(8),
                        ..Default::default()
                    }),
            );
        }

        let header_stats = container(
            column![
                text("Agent Activity Journal")
                    .size(font)
                    .color(theme.text_primary()),
                stats_row
            ]
            .spacing(6),
        )
        .padding(12)
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(header_bg.into()),
            border: iced::Border {
                width: 1.0,
                color: accent_glow,
                radius: iced::border::Radius::from(6),
            },
            ..Default::default()
        });

        content = content.push(header_stats);
        content = content.push(iced::widget::Space::new().height(8));

        // Recent captures timeline
        if activity.captures.is_empty() {
            let empty_state = container(
                column![
                    text("✨ Ready for your first agent session")
                        .size(font)
                        .color(theme.subtext1()),
                    text("Commit with an agent to see activity here")
                        .size(font_small)
                        .color(theme.subtext0())
                ]
                .spacing(4)
                .align_x(iced::Alignment::Center),
            )
            .padding(20)
            .width(Length::Fill)
            .center_x(Length::Fill);

            content = content.push(empty_state);
        } else {
            // Show recent captures
            let recent_captures = activity.recent_captures(50);
            let selected_idx = tab.agent_sidebar.selected_capture_idx;

            for (idx, capture) in recent_captures.iter().enumerate() {
                let is_selected = selected_idx == Some(idx);
                content =
                    content.push(self.view_agent_capture_entry(capture, theme, idx, is_selected));
            }
        }

        scrollable(content).into()
    }

    fn view_agent_capture_entry<'a>(
        &'a self,
        capture: &'a agent::AgentCapture,
        theme: &'a AppTheme,
        idx: usize,
        is_selected: bool,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let font_small = self.ui_font_small();
        let font_tiny = font_small * 0.9;

        let is_reconstructed = capture.is_reconstructed();
        let is_live = !is_reconstructed;

        // Container styling — highlight selected
        let entry_bg = if is_selected {
            theme.surface0()
        } else if is_reconstructed {
            theme.bg_base()
        } else {
            theme.bg_overlay()
        };
        let border_color = if is_selected {
            theme.accent()
        } else if is_reconstructed {
            theme.surface0()
        } else {
            theme.surface1()
        };

        // Format timestamp
        let time_display = if let Some(timestamp) = capture.timestamp_parsed() {
            let elapsed = timestamp.elapsed().unwrap_or(Duration::from_secs(0));
            let secs = elapsed.as_secs();
            if secs >= 24 * 3600 {
                format!("{}d", secs / (24 * 3600))
            } else if secs >= 3600 {
                format!("{}h", secs / 3600)
            } else {
                format!("{}m", secs / 60)
            }
        } else {
            "now".to_string()
        };

        let mut entry_content = Column::new().spacing(2);

        // === Row 1: Description (the most important thing) ===
        if let Some(desc) = capture.description() {
            let truncated: String = if desc.len() > 72 {
                format!("{}…", truncate_str(&desc, 71))
            } else {
                desc
            };
            let desc_color = if is_live {
                theme.text_primary()
            } else {
                theme.subtext1()
            };
            entry_content = entry_content.push(text(truncated).size(font_small).color(desc_color));
        }

        // === Row 2: Badges row — hash, type badge, key stat, timestamp ===
        let mut badges_row = Row::new().spacing(6).align_y(iced::Alignment::Center);

        // Commit hash
        let hash_color = if is_live {
            theme.blue()
        } else {
            theme.overlay0()
        };
        badges_row = badges_row.push(text(capture.short_hash()).size(font_tiny).color(hash_color));

        if is_live {
            // Model badge
            let (model_name, model_badge_color) = if let Some((name, _)) = capture.primary_model() {
                let color = if name.contains("opus") {
                    theme.mauve()
                } else if name.contains("sonnet") {
                    theme.blue()
                } else if name.contains("haiku") || name.contains("gpt") {
                    theme.green()
                } else {
                    theme.overlay1()
                };
                // Short name: "opus-4-6" from "claude-opus-4-6"
                let short = name.split('/').next_back().unwrap_or(name);
                let short = short.strip_prefix("claude-").unwrap_or(short);
                (short, color)
            } else {
                ("unknown", theme.overlay1())
            };
            badges_row = badges_row.push(
                container(text(model_name).size(font_tiny).color(model_badge_color))
                    .padding([1, 5])
                    .style(move |_| container::Style {
                        background: Some(
                            iced::Color {
                                a: 0.15,
                                ..model_badge_color
                            }
                            .into(),
                        ),
                        border: iced::Border::default().rounded(4),
                        ..Default::default()
                    }),
            );

            // Turns count
            badges_row = badges_row.push(
                text(format!("{}t", capture.turns))
                    .size(font_tiny)
                    .color(theme.subtext0()),
            );

            // Duration
            badges_row = badges_row.push(
                text(capture.format_duration())
                    .size(font_tiny)
                    .color(theme.subtext0()),
            );
        } else {
            // Git badge for reconstructed
            let git_color = theme.overlay1();
            badges_row = badges_row.push(
                container(text("git").size(font_tiny).color(git_color))
                    .padding([1, 5])
                    .style(move |_| container::Style {
                        background: Some(
                            iced::Color {
                                a: 0.12,
                                ..git_color
                            }
                            .into(),
                        ),
                        border: iced::Border::default().rounded(4),
                        ..Default::default()
                    }),
            );

            // Diff summary
            let diff = capture.diff_summary();
            if !diff.is_empty() {
                badges_row = badges_row.push(text(diff).size(font_tiny).color(theme.overlay0()));
            }
        }

        // Timestamp pushed to the right
        badges_row = badges_row.push(iced::widget::Space::new().width(Length::Fill));
        badges_row = badges_row.push(text(time_display).size(font_tiny).color(theme.overlay0()));

        entry_content = entry_content.push(badges_row);

        let entry_container = container(entry_content)
            .padding([6, 12])
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(entry_bg.into()),
                border: iced::Border {
                    width: 1.0,
                    color: border_color,
                    radius: iced::border::Radius::from(4),
                },
                ..Default::default()
            });

        // Only make live captures clickable (they have conversations)
        if is_live {
            iced::widget::mouse_area(entry_container)
                .on_press(Event::SelectAgentCapture(idx))
                .into()
        } else {
            entry_container.into()
        }
    }

    fn view_agent_conversation<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let font_small = self.ui_font_small();
        let font_tiny = font_small * 0.9;

        let conversation = match &tab.agent_sidebar.conversation {
            Some(c) => c,
            None => {
                return container(
                    text("Select a session to view its conversation")
                        .size(font_small)
                        .color(theme.subtext0()),
                )
                .padding(20)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into();
            }
        };

        // Get capture info for the header
        let capture = tab
            .agent_sidebar
            .selected_capture_idx
            .and_then(|idx| tab.agent_sidebar.activity.as_ref()?.captures.get(idx));

        let mut content = Column::new().spacing(0);

        // Header with capture info
        if let Some(cap) = capture {
            let header_bg = theme.surface0();
            let desc = cap
                .description()
                .unwrap_or_else(|| cap.short_hash().to_string());
            let desc_truncated: String = if desc.len() > 80 {
                format!("{}…", truncate_str(&desc, 79))
            } else {
                desc
            };

            let mut header_col = Column::new().spacing(4);
            header_col =
                header_col.push(text(desc_truncated).size(font).color(theme.text_primary()));

            // Stats line
            let mut stats_parts = Vec::new();
            stats_parts.push(cap.short_hash().to_string());
            if let Some((name, _)) = cap.primary_model() {
                let short = name.split('/').next_back().unwrap_or(name);
                let short = short.strip_prefix("claude-").unwrap_or(short);
                stats_parts.push(short.to_string());
            }
            stats_parts.push(format!("{}t", cap.turns));
            stats_parts.push(cap.format_duration());
            stats_parts.push(cap.format_cost());

            header_col = header_col.push(
                text(stats_parts.join(" · "))
                    .size(font_tiny)
                    .color(theme.subtext0()),
            );

            content = content.push(
                container(header_col)
                    .padding([10, 16])
                    .width(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(header_bg.into()),
                        ..Default::default()
                    }),
            );
        }

        // Error state
        if let Some(err) = &conversation.error {
            content = content.push(
                container(text(err).size(font_small).color(theme.subtext0()))
                    .padding([16, 16])
                    .width(Length::Fill)
                    .center_x(Length::Fill),
            );
            return scrollable(content)
                .width(Length::Fill)
                .height(Length::Fill)
                .into();
        }

        if conversation.entries.is_empty() {
            content = content.push(
                container(
                    text("No conversation entries found")
                        .size(font_small)
                        .color(theme.subtext0()),
                )
                .padding([16, 16])
                .width(Length::Fill),
            );
            return scrollable(content)
                .width(Length::Fill)
                .height(Length::Fill)
                .into();
        }

        // Render conversation entries
        let mut thread = Column::new().spacing(2).padding([8, 12]);

        for entry in &conversation.entries {
            match entry {
                agent::ConversationEntry::User { text: user_text } => {
                    let user_color = theme.blue();
                    let bg = iced::Color {
                        a: 0.08,
                        ..user_color
                    };

                    let display_text: String = if user_text.len() > 500 {
                        format!("{}…", truncate_str(user_text, 497))
                    } else {
                        user_text.clone()
                    };

                    thread = thread.push(
                        container(
                            column![
                                text("You").size(font_tiny).color(user_color),
                                text(display_text)
                                    .size(font_small)
                                    .color(theme.text_primary())
                            ]
                            .spacing(2),
                        )
                        .padding([8, 12])
                        .width(Length::Fill)
                        .style(move |_| container::Style {
                            background: Some(bg.into()),
                            border: iced::Border {
                                width: 0.0,
                                color: iced::Color::TRANSPARENT,
                                radius: iced::border::Radius::from(6),
                            },
                            ..Default::default()
                        }),
                    );
                }
                agent::ConversationEntry::Assistant {
                    text: asst_text,
                    model,
                } => {
                    let model_short = model.strip_prefix("claude-").unwrap_or(model);
                    let asst_color = theme.mauve();
                    let bg = iced::Color {
                        a: 0.05,
                        ..asst_color
                    };

                    let display_text: String = if asst_text.len() > 1000 {
                        format!("{}…", truncate_str(asst_text, 997))
                    } else {
                        asst_text.clone()
                    };

                    thread = thread.push(
                        container(
                            column![
                                text(model_short).size(font_tiny).color(asst_color),
                                text(display_text)
                                    .size(font_small)
                                    .color(theme.text_secondary())
                            ]
                            .spacing(2),
                        )
                        .padding([8, 12])
                        .width(Length::Fill)
                        .style(move |_| container::Style {
                            background: Some(bg.into()),
                            border: iced::Border {
                                width: 0.0,
                                color: iced::Color::TRANSPARENT,
                                radius: iced::border::Radius::from(6),
                            },
                            ..Default::default()
                        }),
                    );
                }
                agent::ConversationEntry::ToolCall { tool, summary } => {
                    let tool_color = theme.yellow();

                    let label = format!("→ {} {}", tool, summary);
                    let label_truncated: String = if label.len() > 100 {
                        format!("{}…", truncate_str(&label, 97))
                    } else {
                        label
                    };

                    thread = thread.push(
                        container(text(label_truncated).size(font_tiny).color(tool_color))
                            .padding([3, 12]),
                    );
                }
                agent::ConversationEntry::ToolResult {
                    tool: _,
                    output,
                    is_error,
                } => {
                    if !output.is_empty() {
                        let result_color = if *is_error {
                            theme.red()
                        } else {
                            theme.overlay1()
                        };
                        let prefix = if *is_error { "✗ " } else { "" };
                        let bg = if *is_error {
                            iced::Color {
                                a: 0.06,
                                ..theme.red()
                            }
                        } else {
                            theme.bg_base()
                        };

                        // Only show first few lines of output
                        let lines: Vec<&str> = output.lines().take(6).collect();
                        let display = if output.lines().count() > 6 {
                            format!("{}{}…", prefix, lines.join("\n"))
                        } else {
                            format!("{}{}", prefix, lines.join("\n"))
                        };

                        thread = thread.push(
                            container(text(display).size(font_tiny).color(result_color))
                                .padding([4, 12])
                                .width(Length::Fill)
                                .style(move |_| container::Style {
                                    background: Some(bg.into()),
                                    border: iced::Border {
                                        width: 0.0,
                                        color: iced::Color::TRANSPARENT,
                                        radius: iced::border::Radius::from(4),
                                    },
                                    ..Default::default()
                                }),
                        );
                    }
                }
                agent::ConversationEntry::Thinking { text: think_text } => {
                    let think_color = theme.overlay0();
                    let display: String = if think_text.len() > 300 {
                        format!("💭 {}…", truncate_str(think_text, 297))
                    } else {
                        format!("💭 {}", think_text)
                    };

                    thread = thread.push(
                        container(text(display).size(font_tiny).color(think_color))
                            .padding([3, 12]),
                    );
                }
                agent::ConversationEntry::Compaction { summary } => {
                    let compact_color = theme.peach();
                    let display: String = if summary.len() > 200 {
                        format!("⟳ Context compacted: {}…", truncate_str(summary, 197))
                    } else {
                        format!("⟳ Context compacted: {}", summary)
                    };

                    thread = thread.push(
                        container(text(display).size(font_tiny).color(compact_color))
                            .padding([6, 12]),
                    );
                }
            }
        }

        content = content.push(thread);

        scrollable(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn view_claude_sidebar<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let config = &tab.claude_config;

        let mut content = Column::new().spacing(0);

        // Skills section
        content = content.push(self.view_claude_section(
            "Skills",
            "skills",
            &config.skills,
            config.expanded.contains("skills"),
            theme.success(),
            &config.selected_item,
        ));

        // Plugins section
        content = content.push(self.view_claude_section(
            "Plugins",
            "plugins",
            &config.plugins,
            config.expanded.contains("plugins"),
            theme.accent(),
            &config.selected_item,
        ));

        // MCP Servers section
        content = content.push(self.view_claude_section(
            "MCP Servers",
            "mcp_servers",
            &config.mcp_servers,
            config.expanded.contains("mcp_servers"),
            theme.peach(),
            &config.selected_item,
        ));

        // Hooks section
        content = content.push(self.view_claude_section(
            "Hooks",
            "hooks",
            &config.hooks,
            config.expanded.contains("hooks"),
            theme.mauve(),
            &config.selected_item,
        ));

        // Settings section
        content = content.push(self.view_claude_section(
            "Settings",
            "settings",
            &config.settings,
            config.expanded.contains("settings"),
            theme.overlay1(),
            &config.selected_item,
        ));

        scrollable(content)
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
    }

    fn view_claude_section<'a>(
        &'a self,
        label: &'a str,
        key: &'a str,
        items: &'a [ClaudeConfigItem],
        expanded: bool,
        dot_color: iced::Color,
        selected: &Option<(String, usize)>,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let font_small = self.ui_font_small();
        let hover_bg = theme.surface0();

        let chevron = if expanded { "\u{25BC}" } else { "\u{25B6}" };
        let count_text = format!("{}", items.len());

        // Count badge
        let badge_bg = iced::Color {
            a: 0.15,
            ..theme.text_muted()
        };
        let muted = theme.text_muted();
        let badge = container(text(count_text).size(font_small).color(muted))
            .padding([1, 6])
            .style(move |_| container::Style {
                background: Some(badge_bg.into()),
                border: iced::Border {
                    radius: 8.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            });

        let header_row = row![
            text(chevron).size(font_small).color(theme.text_secondary()),
            iced::widget::Space::new().width(Length::Fixed(6.0)),
            text(label).size(font).color(theme.text_primary()),
            iced::widget::Space::new().width(Length::Fill),
            badge,
        ]
        .align_y(iced::Alignment::Center)
        .padding([6, 10]);

        let header_btn = button(header_row)
            .style(move |_theme, status| {
                let bg = if matches!(status, button::Status::Hovered) {
                    Some(hover_bg.into())
                } else {
                    Some(iced::Color::TRANSPARENT.into())
                };
                button::Style {
                    background: bg,
                    border: iced::Border::default(),
                    text_color: iced::Color::WHITE,
                    ..Default::default()
                }
            })
            .padding(0)
            .width(Length::Fill)
            .on_press(Event::ToggleClaudeSection(key.to_string()));

        let mut section = Column::new().spacing(0);
        section = section.push(header_btn);

        if expanded {
            for (idx, item) in items.iter().enumerate() {
                let is_selected = selected
                    .as_ref()
                    .map(|(s, i)| s == key && *i == idx)
                    .unwrap_or(false);
                section =
                    section.push(self.view_claude_item(key, idx, item, dot_color, is_selected));
            }

            if items.is_empty() {
                let empty_row = container(
                    text("None found")
                        .size(font_small)
                        .color(theme.text_muted()),
                )
                .padding(iced::Padding {
                    top: 4.0,
                    right: 10.0,
                    bottom: 4.0,
                    left: 28.0,
                });
                section = section.push(empty_row);
            }
        }

        section.into()
    }

    fn view_claude_item<'a>(
        &'a self,
        section_key: &'a str,
        idx: usize,
        item: &'a ClaudeConfigItem,
        dot_color: iced::Color,
        is_selected: bool,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let font_small = self.ui_font_small();

        let selected_bg = if is_selected {
            theme.surface0()
        } else {
            iced::Color::TRANSPARENT
        };
        let accent = theme.accent();
        let left_border_color = if is_selected {
            accent
        } else {
            iced::Color::TRANSPARENT
        };
        let hover_bg = theme.surface0();

        // Scope badge text
        let scope_str = match item.scope {
            ConfigScope::User => "USR",
            ConfigScope::Project => "PRJ",
        };

        let scope_bg = iced::Color {
            a: 0.12,
            ..theme.text_muted()
        };
        let scope_text_color = theme.text_muted();
        let scope_badge = container(
            text(scope_str)
                .size(font_small - 1.0)
                .color(scope_text_color),
        )
        .padding([1, 4])
        .style(move |_| container::Style {
            background: Some(scope_bg.into()),
            border: iced::Border {
                radius: 4.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });

        // Dot
        let dot = container(iced::widget::Space::new())
            .width(Length::Fixed(6.0))
            .height(Length::Fixed(6.0))
            .style(move |_| container::Style {
                background: Some(dot_color.into()),
                border: iced::Border {
                    radius: 3.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            });

        let item_row = row![
            dot,
            iced::widget::Space::new().width(Length::Fixed(8.0)),
            text(&item.name).size(font).color(theme.text_primary()),
            iced::widget::Space::new().width(Length::Fill),
            scope_badge,
        ]
        .align_y(iced::Alignment::Center)
        .padding(iced::Padding {
            top: 4.0,
            right: 10.0,
            bottom: 4.0,
            left: 24.0,
        });

        let item_btn = button(item_row)
            .style(move |_theme, status| {
                let bg = if is_selected {
                    Some(selected_bg.into())
                } else if matches!(status, button::Status::Hovered) {
                    Some(hover_bg.into())
                } else {
                    Some(iced::Color::TRANSPARENT.into())
                };
                button::Style {
                    background: bg,
                    border: iced::Border::default(),
                    text_color: iced::Color::WHITE,
                    ..Default::default()
                }
            })
            .padding(0)
            .width(Length::Fill)
            .on_press(Event::ClaudeItemSelect(section_key.to_string(), idx));

        // Wrap with left accent border when selected
        let left_border = container(iced::widget::Space::new())
            .width(Length::Fixed(2.0))
            .height(Length::Fill)
            .style(move |_| container::Style {
                background: Some(left_border_color.into()),
                ..Default::default()
            });

        row![left_border, item_btn].height(Length::Shrink).into()
    }

    fn view_file_content<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let font_small = self.ui_font_small();
        let mut content = Column::new().spacing(0);

        let Some(fv) = tab.file_viewer() else {
            return container(text("(no file)").size(font).color(theme.text_secondary()))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into();
        };

        // Header with filename and close button
        let file_name = fv
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let rel_path = fv
            .path
            .strip_prefix(&tab.repo_path)
            .ok()
            .map(|p| p.display().to_string())
            .unwrap_or(file_name.clone());

        // Determine file viewer mode by extension/state.
        let is_markdown = TabState::is_markdown_file(&fv.path);
        let is_html = TabState::is_html_file(&fv.path);
        #[cfg(feature = "excalidraw")]
        let is_excalidraw = excalidraw::is_excalidraw_file(&fv.path);
        #[cfg(not(feature = "excalidraw"))]
        let is_excalidraw = false;
        let has_inline_webview =
            fv.webview_content.is_some() && (is_markdown || is_html || is_excalidraw);

        let header_bg = theme.bg_overlay();
        let ghost = self.ghost_button_style();
        let ghost2 = self.ghost_button_style();
        let ghost3 = self.ghost_button_style();
        let header = if is_markdown || is_html || is_excalidraw {
            // Markdown header with "View in Browser" button for Mermaid support
            row![
                text(rel_path).size(font).color(theme.text_primary()),
                iced::widget::Space::new().width(Length::Fill),
                button(text("View in Browser").size(font))
                    .style(ghost)
                    .padding([4, 12])
                    .on_press(Event::OpenMarkdownInBrowser),
                iced::widget::Space::new().width(Length::Fixed(4.0)),
                text("Esc: close")
                    .size(font_small)
                    .color(theme.text_secondary()),
                iced::widget::Space::new().width(Length::Fixed(8.0)),
                button(text("Close").size(font))
                    .style(ghost2)
                    .padding([4, 12])
                    .on_press(Event::CloseFileView),
            ]
            .padding(8)
            .spacing(8)
        } else {
            row![
                text(rel_path).size(font).color(theme.text_primary()),
                iced::widget::Space::new().width(Length::Fill),
                button(text("Copy All").size(font))
                    .style(ghost)
                    .padding([4, 12])
                    .on_press(Event::CopyFileContent),
                iced::widget::Space::new().width(Length::Fixed(4.0)),
                button(text("Open in Browser").size(font))
                    .style(ghost2)
                    .padding([4, 12])
                    .on_press(Event::OpenFileInBrowser),
                iced::widget::Space::new().width(Length::Fixed(4.0)),
                text("Esc: close")
                    .size(font_small)
                    .color(theme.text_secondary()),
                iced::widget::Space::new().width(Length::Fixed(8.0)),
                button(text("Close").size(font))
                    .style(ghost3)
                    .padding([4, 12])
                    .on_press(Event::CloseFileView),
            ]
            .padding(8)
            .spacing(8)
        };

        content =
            content.push(
                container(header)
                    .width(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(header_bg.into()),
                        ..Default::default()
                    }),
            );

        if let Some(notice) = &tab.diff_syntax_notice {
            content = content.push(
                container(text(notice).size(font_small).color(theme.warning()))
                    .width(Length::Fill)
                    .padding([6, 10])
                    .style(move |_| container::Style {
                        background: Some(theme.bg_overlay().into()),
                        border: iced::Border {
                            width: 1.0,
                            color: theme.surface0(),
                            radius: 0.0.into(),
                        },
                        ..Default::default()
                    }),
            );
        }

        if let Some(notice) = &fv.preview_notice {
            let notice_bg = theme.bg_overlay();
            let notice_border = theme.surface0();
            content = content.push(
                container(text(notice).size(font_small).color(theme.warning()))
                    .width(Length::Fill)
                    .padding([6, 10])
                    .style(move |_| container::Style {
                        background: Some(notice_bg.into()),
                        border: iced::Border {
                            width: 1.0,
                            color: notice_border,
                            radius: 0.0.into(),
                        },
                        ..Default::default()
                    }),
            );
        }

        if let Some(notice) = &fv.syntax_highlight_notice {
            let notice_bg = theme.bg_overlay();
            let notice_border = theme.surface0();
            content = content.push(
                container(text(notice).size(font_small).color(theme.text_secondary()))
                    .width(Length::Fill)
                    .padding([6, 10])
                    .style(move |_| container::Style {
                        background: Some(notice_bg.into()),
                        border: iced::Border {
                            width: 1.0,
                            color: notice_border,
                            radius: 0.0.into(),
                        },
                        ..Default::default()
                    }),
            );
        }
        let waiting_for_file_load = fv.load_in_progress
            && fv.file_content.is_empty()
            && fv.image_handle.is_none()
            && fv.webview_content.is_none();
        let waiting_for_initial_syntax = fv.syntax_highlight_in_progress
            && fv.syntax_highlight_lines.is_none()
            && !fv.file_content.is_empty()
            && fv.webview_content.is_none()
            && fv.image_handle.is_none();
        let show_file_loading_message = waiting_for_file_load
            && fv.load_started_at.is_some_and(|started| {
                started.elapsed() >= Duration::from_millis(LOADING_INDICATOR_DELAY_MS)
            });
        let show_initial_syntax_message = waiting_for_initial_syntax
            && fv.load_started_at.is_some_and(|started| {
                started.elapsed() >= Duration::from_millis(LOADING_INDICATOR_DELAY_MS)
            });

        // Check if we're viewing an image
        if waiting_for_file_load {
            if show_file_loading_message {
                content = content.push(
                    container(
                        text("Loading file...")
                            .size(font)
                            .color(theme.text_secondary()),
                    )
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill)
                    .padding(16),
                );
            } else {
                content = content.push(iced::widget::Space::new().height(Length::Fill));
            }
        } else if waiting_for_initial_syntax {
            if show_initial_syntax_message {
                content = content.push(
                    container(
                        text("Highlighting syntax...")
                            .size(font)
                            .color(theme.text_secondary()),
                    )
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill)
                    .padding(16),
                );
            } else {
                content = content.push(iced::widget::Space::new().height(Length::Fill));
            }
        } else if let Some(handle) = &fv.image_handle {
            // Display image
            let img = image(handle.clone()).content_fit(iced::ContentFit::Contain);

            content = content.push(
                scrollable(
                    container(img)
                        .width(Length::Fill)
                        .center_x(Length::Fill)
                        .padding(16),
                )
                .height(Length::Fill)
                .width(Length::Fill),
            );
        } else if has_inline_webview {
            // Excalidraw, Mermaid-markdown, and HTML render inline via WebView.
            content = content.push(iced::widget::Space::new().height(Length::Fill));
        } else if is_markdown && fv.preview_notice.is_none() && !fv.file_content.is_empty() {
            // Fallback markdown rendering when HTML preview is unavailable.
            content = content.push(self.view_markdown_content(tab));
        } else if is_markdown || is_html || is_excalidraw {
            let msg = fv
                .preview_notice
                .as_deref()
                .unwrap_or("Inline preview unavailable for this file. Click \"View in Browser\".");
            content = content.push(
                container(text(msg).size(font).color(theme.text_secondary()))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill)
                    .padding(16),
            );
        } else {
            // File content with line numbers
            let render_started_at = Instant::now();
            let mut file_column = Column::new().spacing(0);
            let mono = iced::Font::MONOSPACE;
            let has_syntax_lines = fv.syntax_highlight_lines.is_some();
            let total_line_count = fv.file_content.lines().count();
            let render_line_limit = if has_syntax_lines {
                MAX_FILE_VIEW_RENDER_LINES_WITH_SYNTAX
            } else {
                MAX_FILE_VIEW_RENDER_LINES
            };
            let render_line_count = total_line_count.min(render_line_limit);

            if total_line_count > render_line_count {
                let hidden_lines = total_line_count.saturating_sub(render_line_count);
                let render_notice = format!(
                    "Rendering first {} of {} lines ({} hidden) for performance.",
                    render_line_count, total_line_count, hidden_lines
                );
                let notice_bg = theme.bg_overlay();
                let notice_border = theme.surface0();
                content = content.push(
                    container(
                        text(render_notice)
                            .size(font_small)
                            .color(theme.text_secondary()),
                    )
                    .width(Length::Fill)
                    .padding([6, 10])
                    .style(move |_| container::Style {
                        background: Some(notice_bg.into()),
                        border: iced::Border {
                            width: 1.0,
                            color: notice_border,
                            radius: 0.0.into(),
                        },
                        ..Default::default()
                    }),
                );
            }

            for (i, line) in fv.file_content.lines().take(render_line_count).enumerate() {
                let line_num = format!("{:4}", i + 1);
                let shown_line = if line.is_empty() { " " } else { line };

                let line_body: Element<'_, Event, Theme, iced::Renderer> =
                    if let Some(highlighted_line) = fv
                        .syntax_highlight_lines
                        .as_ref()
                        .and_then(|lines| lines.get(i))
                    {
                        let mut content_row = Row::new().spacing(0);
                        for segment in &highlighted_line.segments {
                            content_row = content_row.push(
                                text(segment.text.as_str())
                                    .size(font)
                                    .color(segment.color)
                                    .font(mono),
                            );
                        }
                        if highlighted_line.segments.is_empty() {
                            content_row = content_row.push(
                                text(shown_line)
                                    .size(font)
                                    .color(theme.text_primary())
                                    .font(mono),
                            );
                        }
                        container(content_row).width(Length::Fill).into()
                    } else {
                        text(shown_line)
                            .size(font)
                            .color(theme.text_primary())
                            .font(mono)
                            .into()
                    };

                let line_row = row![
                    text(line_num)
                        .size(font)
                        .color(theme.text_muted())
                        .font(mono),
                    text(" ").size(font).font(mono),
                    line_body,
                ]
                .spacing(0);

                file_column =
                    file_column.push(container(line_row).width(Length::Fill).padding([1, 4]));
            }

            if total_line_count > render_line_count {
                file_column = file_column.push(
                    container(
                        text(format!(
                            "... {} additional lines not rendered",
                            total_line_count.saturating_sub(render_line_count)
                        ))
                        .size(font_small)
                        .color(theme.text_muted()),
                    )
                    .width(Length::Fill)
                    .padding([6, 4]),
                );
            }

            if fv.file_content.is_empty() {
                file_column = file_column.push(
                    text("(empty file)")
                        .size(font)
                        .color(theme.text_secondary()),
                );
            }

            maybe_log_file_view_build(
                Some(fv.path.as_path()),
                total_line_count,
                render_line_count,
                has_syntax_lines,
                render_started_at.elapsed(),
            );

            content = content.push(
                scrollable(file_column.padding(8))
                    .id(file_view_scrollable_id())
                    .on_scroll({
                        let tab_id = tab.id;
                        move |viewport| Event::FileViewScrolled(tab_id, viewport)
                    })
                    .height(Length::Fill)
                    .width(Length::Fill),
            );
        }

        let bg = theme.bg_base();
        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                ..Default::default()
            })
            .into()
    }

    fn view_markdown_content<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let mut content = Column::new().spacing(8).padding(16);

        let Some(fv) = tab.file_viewer() else {
            return content.into();
        };

        let mut in_code_block = false;
        let mut in_mermaid_block = false;
        let mut code_block_content: Vec<String> = Vec::new();
        let mut in_list = false;
        let mut table_rows: Vec<Vec<String>> = Vec::new();
        let mut table_has_header = false;

        for line in fv.file_content.lines() {
            let trimmed = line.trim();

            // Table row accumulation — detect end of table and render
            if !table_rows.is_empty() && !trimmed.starts_with('|') {
                // End of table — render it
                content = content.push(self.view_markdown_table(&table_rows, table_has_header));
                table_rows.clear();
                table_has_header = false;
            }

            // Handle code blocks
            if trimmed.starts_with("```") {
                if in_mermaid_block {
                    // End of mermaid block - just close it (placeholder already shown)
                    in_mermaid_block = false;
                    continue;
                } else if in_code_block {
                    // End of code block - render accumulated content
                    let code_bg = theme.bg_overlay();
                    // Create an owned string for the text widget
                    let code_content: String = code_block_content.join("\n");
                    let mut code_col = Column::new().spacing(0);
                    for code_line in code_content.lines() {
                        code_col = code_col.push(
                            text(code_line.to_string())
                                .size(font - 1.0)
                                .font(iced::Font::MONOSPACE)
                                .color(theme.text_primary()),
                        );
                    }
                    content =
                        content.push(container(code_col).width(Length::Fill).padding(12).style(
                            move |_| container::Style {
                                background: Some(code_bg.into()),
                                border: iced::Border {
                                    radius: 6.0.into(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            },
                        ));
                    code_block_content.clear();
                    in_code_block = false;
                } else {
                    // Start of code block - check for mermaid
                    let lang = trimmed.strip_prefix("```").unwrap_or("");
                    if lang == "mermaid" {
                        // Show a placeholder for mermaid diagrams
                        content = content.push(
                            container(
                                column![
                                    text("Mermaid Diagram").size(font).color(theme.accent()),
                                    text("Click \"View in Browser\" to see the rendered diagram")
                                        .size(font - 2.0)
                                        .color(theme.text_secondary()),
                                ]
                                .spacing(4)
                                .align_x(iced::Alignment::Center),
                            )
                            .width(Length::Fill)
                            .padding(24)
                            .style(move |_| container::Style {
                                background: Some(theme.bg_overlay().into()),
                                border: iced::Border {
                                    radius: 6.0.into(),
                                    color: theme.accent(),
                                    width: 1.0,
                                },
                                ..Default::default()
                            }),
                        );
                        // Skip content until closing ```
                        in_mermaid_block = true;
                    } else {
                        in_code_block = true;
                    }
                }
                continue;
            }

            // Skip mermaid block content
            if in_mermaid_block {
                continue;
            }

            if in_code_block {
                code_block_content.push(line.to_string());
                continue;
            }

            // Headers
            if trimmed.starts_with("######") {
                let header_text = trimmed.strip_prefix("######").unwrap_or("").trim();
                content = content.push(text(header_text).size(font).color(theme.text_primary()));
            } else if trimmed.starts_with("#####") {
                let header_text = trimmed.strip_prefix("#####").unwrap_or("").trim();
                content = content.push(
                    text(header_text)
                        .size(font + 1.0)
                        .color(theme.text_primary()),
                );
            } else if trimmed.starts_with("####") {
                let header_text = trimmed.strip_prefix("####").unwrap_or("").trim();
                content = content.push(
                    text(header_text)
                        .size(font + 2.0)
                        .color(theme.text_primary()),
                );
            } else if trimmed.starts_with("###") {
                let header_text = trimmed.strip_prefix("###").unwrap_or("").trim();
                content = content.push(
                    text(header_text)
                        .size(font + 4.0)
                        .color(theme.text_primary()),
                );
            } else if trimmed.starts_with("##") {
                let header_text = trimmed.strip_prefix("##").unwrap_or("").trim();
                let border_color = theme.border();
                content = content.push(
                    column![
                        text(header_text)
                            .size(font + 6.0)
                            .color(theme.text_primary()),
                        container(iced::widget::Space::new())
                            .width(Length::Fill)
                            .height(Length::Fixed(1.0))
                            .style(move |_| container::Style {
                                background: Some(border_color.into()),
                                ..Default::default()
                            }),
                    ]
                    .spacing(4),
                );
            } else if trimmed.starts_with('#') && !trimmed.starts_with("##") {
                let header_text = trimmed.strip_prefix('#').unwrap_or("").trim();
                let border_color = theme.border();
                content = content.push(
                    column![
                        text(header_text)
                            .size(font + 10.0)
                            .color(theme.text_primary()),
                        container(iced::widget::Space::new())
                            .width(Length::Fill)
                            .height(Length::Fixed(1.0))
                            .style(move |_| container::Style {
                                background: Some(border_color.into()),
                                ..Default::default()
                            }),
                    ]
                    .spacing(4),
                );
            }
            // Blockquotes
            else if trimmed.starts_with('>') {
                let quote_text = trimmed.strip_prefix('>').unwrap_or("").trim();
                let border_color = theme.border();
                content = content.push(
                    container(self.parse_inline_markdown(quote_text, font))
                        .padding([8, 16])
                        .style(move |_| container::Style {
                            border: iced::Border {
                                color: border_color,
                                width: 0.0,
                                radius: 0.0.into(),
                            },
                            ..Default::default()
                        }),
                );
            }
            // Horizontal rule
            else if trimmed == "---" || trimmed == "***" || trimmed == "___" {
                let border_color = theme.border();
                content = content.push(
                    container(iced::widget::Space::new())
                        .width(Length::Fill)
                        .height(Length::Fixed(1.0))
                        .style(move |_| container::Style {
                            background: Some(border_color.into()),
                            ..Default::default()
                        }),
                );
            }
            // Unordered lists
            else if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
                let list_text = &trimmed[2..];
                content = content.push(
                    row![
                        text("  \u{2022}  ")
                            .size(font)
                            .color(theme.text_secondary()),
                        self.parse_inline_markdown(list_text, font),
                    ]
                    .spacing(0),
                );
                in_list = true;
            }
            // Task lists
            else if trimmed.starts_with("- [ ] ")
                || trimmed.starts_with("- [x] ")
                || trimmed.starts_with("- [X] ")
            {
                let is_checked = trimmed.starts_with("- [x]") || trimmed.starts_with("- [X]");
                let task_text = &trimmed[6..];
                let checkbox = if is_checked { "\u{2611}" } else { "\u{2610}" };
                content = content.push(
                    row![
                        text(format!("  {}  ", checkbox))
                            .size(font)
                            .color(if is_checked {
                                theme.success()
                            } else {
                                theme.text_secondary()
                            }),
                        text(task_text).size(font).color(theme.text_primary()),
                    ]
                    .spacing(0),
                );
            }
            // Ordered lists (basic)
            else if trimmed.len() > 2
                && trimmed
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
                && trimmed.contains(". ")
            {
                if let Some(pos) = trimmed.find(". ") {
                    let num = &trimmed[..pos];
                    let list_text = &trimmed[pos + 2..];
                    content = content.push(
                        row![
                            text(format!("  {}.  ", num))
                                .size(font)
                                .color(theme.text_secondary()),
                            self.parse_inline_markdown(list_text, font),
                        ]
                        .spacing(0),
                    );
                }
            }
            // Table rows
            else if trimmed.starts_with('|') {
                // Separator row (|---|---|)
                let is_separator = trimmed.contains("---");
                if is_separator {
                    table_has_header = true;
                } else {
                    let cells: Vec<String> = trimmed
                        .split('|')
                        .filter(|s| !s.is_empty())
                        .map(|s| s.trim().to_string())
                        .collect();
                    if !cells.is_empty() {
                        table_rows.push(cells);
                    }
                }
            }
            // Empty line
            else if trimmed.is_empty() {
                if in_list {
                    in_list = false;
                }
                content = content.push(iced::widget::Space::new().height(Length::Fixed(8.0)));
            }
            // Regular paragraph
            else {
                content = content.push(self.parse_inline_markdown(line, font));
            }
        }

        // Flush any remaining table
        if !table_rows.is_empty() {
            content = content.push(self.view_markdown_table(&table_rows, table_has_header));
        }

        scrollable(content)
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
    }

    fn view_markdown_table<'a>(
        &'a self,
        rows: &[Vec<String>],
        has_header: bool,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let font_small = font - 1.0;
        let border_color = theme.border();
        let header_bg = theme.bg_surface();
        let alt_bg = iced::Color {
            a: 0.3,
            ..theme.bg_surface()
        };

        let mut table_col = Column::new().spacing(0);

        for (row_idx, cells) in rows.iter().enumerate() {
            let is_header = has_header && row_idx == 0;
            let is_even = row_idx % 2 == 0;

            let mut row_widget = Row::new().spacing(0);

            for cell in cells {
                let cell_content: Element<'a, Event, Theme, iced::Renderer> = if is_header {
                    text(cell.clone())
                        .size(font_small)
                        .color(theme.text_primary())
                        .font(iced::Font {
                            weight: iced::font::Weight::Bold,
                            ..Default::default()
                        })
                        .into()
                } else {
                    self.parse_inline_markdown(cell, font_small)
                };

                let cell_bg = if is_header {
                    header_bg
                } else if !is_even {
                    alt_bg
                } else {
                    iced::Color::TRANSPARENT
                };

                let cell_container = container(cell_content)
                    .padding([6, 12])
                    .width(Length::FillPortion(1))
                    .style(move |_| container::Style {
                        background: Some(cell_bg.into()),
                        border: iced::Border {
                            color: border_color,
                            width: 0.5,
                            radius: 0.0.into(),
                        },
                        ..Default::default()
                    });

                row_widget = row_widget.push(cell_container);
            }

            table_col = table_col.push(row_widget);
        }

        let table_border = theme.border();
        container(table_col)
            .width(Length::Fill)
            .style(move |_| container::Style {
                border: iced::Border {
                    color: table_border,
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            })
            .into()
    }

    fn parse_inline_markdown<'a>(
        &'a self,
        input: &str,
        font_size: f32,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let text_color = theme.text_primary();
        let code_bg = theme.bg_overlay();
        let accent_color = theme.accent();
        let secondary_color = theme.text_secondary();
        let mono = iced::Font::MONOSPACE;
        let bold_font = iced::Font {
            weight: iced::font::Weight::Bold,
            ..Default::default()
        };

        // Quick check: if no special chars, return plain text
        if !input.contains('`')
            && !input.contains('*')
            && !input.contains('_')
            && !input.contains('[')
        {
            return text(input.to_string())
                .size(font_size)
                .color(text_color)
                .into();
        }

        type Span<'s> = iced::advanced::text::Span<'s, (), iced::Font>;

        let mut spans: Vec<Span<'_>> = Vec::new();
        let chars: Vec<char> = input.chars().collect();
        let len = chars.len();
        let mut i = 0;
        let mut current = String::new();

        while i < len {
            // Inline code: `code`
            if chars[i] == '`' {
                if !current.is_empty() {
                    spans.push(Span::new(current.clone()).color(text_color));
                    current.clear();
                }
                i += 1;
                let mut code = String::new();
                while i < len && chars[i] != '`' {
                    code.push(chars[i]);
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
                let mut code_span = Span::new(code)
                    .font(mono)
                    .size(font_size - 1.0)
                    .color(text_color)
                    .padding([0, 4]);
                code_span.highlight = Some(iced::advanced::text::Highlight {
                    background: code_bg.into(),
                    border: iced::Border {
                        radius: 3.0.into(),
                        ..Default::default()
                    },
                });
                spans.push(code_span);
            }
            // Bold: **text** or __text__
            else if i + 1 < len
                && ((chars[i] == '*' && chars[i + 1] == '*')
                    || (chars[i] == '_' && chars[i + 1] == '_'))
            {
                let marker = chars[i];
                if !current.is_empty() {
                    spans.push(Span::new(current.clone()).color(text_color));
                    current.clear();
                }
                i += 2;
                let mut bold_text = String::new();
                while i + 1 < len && !(chars[i] == marker && chars[i + 1] == marker) {
                    bold_text.push(chars[i]);
                    i += 1;
                }
                if i + 1 < len {
                    i += 2;
                }
                // Parse inner content for nested inline code
                if bold_text.contains('`') {
                    let inner_chars: Vec<char> = bold_text.chars().collect();
                    let inner_len = inner_chars.len();
                    let mut j = 0;
                    let mut inner_current = String::new();
                    while j < inner_len {
                        if inner_chars[j] == '`' {
                            if !inner_current.is_empty() {
                                spans.push(
                                    Span::new(inner_current.clone())
                                        .color(text_color)
                                        .font(bold_font),
                                );
                                inner_current.clear();
                            }
                            j += 1;
                            let mut code = String::new();
                            while j < inner_len && inner_chars[j] != '`' {
                                code.push(inner_chars[j]);
                                j += 1;
                            }
                            if j < inner_len {
                                j += 1;
                            }
                            let mut code_span = Span::new(code)
                                .font(iced::Font {
                                    weight: iced::font::Weight::Bold,
                                    ..mono
                                })
                                .size(font_size - 1.0)
                                .color(text_color)
                                .padding([0, 4]);
                            code_span.highlight = Some(iced::advanced::text::Highlight {
                                background: code_bg.into(),
                                border: iced::Border {
                                    radius: 3.0.into(),
                                    ..Default::default()
                                },
                            });
                            spans.push(code_span);
                        } else {
                            inner_current.push(inner_chars[j]);
                            j += 1;
                        }
                    }
                    if !inner_current.is_empty() {
                        spans.push(Span::new(inner_current).color(text_color).font(bold_font));
                    }
                } else {
                    spans.push(Span::new(bold_text).color(text_color).font(bold_font));
                }
            }
            // Italic: *text* or _text_ (single, not followed by same or space)
            else if (chars[i] == '*' || chars[i] == '_')
                && (i + 1 < len && chars[i + 1] != chars[i] && !chars[i + 1].is_whitespace())
            {
                let marker = chars[i];
                if !current.is_empty() {
                    spans.push(Span::new(current.clone()).color(text_color));
                    current.clear();
                }
                i += 1;
                let mut italic_text = String::new();
                while i < len && chars[i] != marker {
                    italic_text.push(chars[i]);
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
                spans.push(Span::new(italic_text).color(secondary_color));
            }
            // Link: [text](url)
            else if chars[i] == '[' {
                if !current.is_empty() {
                    spans.push(Span::new(current.clone()).color(text_color));
                    current.clear();
                }
                i += 1;
                let mut link_text = String::new();
                while i < len && chars[i] != ']' {
                    link_text.push(chars[i]);
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
                if i < len && chars[i] == '(' {
                    i += 1;
                    while i < len && chars[i] != ')' {
                        i += 1;
                    }
                    if i < len {
                        i += 1;
                    }
                }
                spans.push(Span::new(link_text).color(accent_color).underline(true));
            } else {
                current.push(chars[i]);
                i += 1;
            }
        }

        if !current.is_empty() {
            spans.push(Span::new(current).color(text_color));
        }

        iced::widget::text::Rich::with_spans(spans)
            .size(font_size)
            .width(Length::Fill)
            .into()
    }

    fn view_git_list<'a>(&'a self, tab: &'a TabState) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        // Only show the loading banner for the initial fetch. Subsequent polls refresh in place
        // to avoid visible flashing in the Git list.
        let show_loading = tab.git_status_loading && tab.last_git_status_hash.is_none();

        // Debug large git status results
        let total_files = tab.staged.len() + tab.unstaged.len() + tab.untracked.len();
        if total_files > 1000 {
            freeze_debug!("Large git status with {} files ({} staged, {} unstaged, {} untracked) in view_git_list",
                total_files, tab.staged.len(), tab.unstaged.len(), tab.untracked.len());
        }

        let mut content = Column::new().spacing(8).padding(8);

        // Branch display - styled rounded container with diamond icon
        if tab.is_git_repo {
            let branch_bg = theme.bg_base();
            let mauve = theme.mauve();
            let branch_container = container(
                row![
                    text("\u{25c6}").size(font).color(mauve),
                    text(&tab.branch_name)
                        .size(font)
                        .color(mauve)
                        .font(iced::Font::with_name("Menlo")),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center),
            )
            .padding([4, 10])
            .style(move |_| container::Style {
                background: Some(branch_bg.into()),
                border: iced::Border {
                    radius: 6.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            });
            content = content.push(branch_container);
        }

        if show_loading {
            content = content.push(
                text("Loading git status...")
                    .size(font - 1.0)
                    .color(theme.text_secondary()),
            );
        }

        if !tab.staged.is_empty() {
            content = content.push(
                row![
                    text("S T A G E D").size(10).color(theme.overlay0()),
                    text(format!("{}", tab.staged.len()))
                        .size(10)
                        .color(theme.success()),
                ]
                .spacing(6),
            );
            for file in &tab.staged {
                content = content.push(self.view_file_item(file, tab));
            }
        }

        if !tab.unstaged.is_empty() {
            content = content.push(
                row![
                    text("U N S T A G E D").size(10).color(theme.overlay0()),
                    text(format!("{}", tab.unstaged.len()))
                        .size(10)
                        .color(theme.warning()),
                ]
                .spacing(6),
            );
            for file in &tab.unstaged {
                content = content.push(self.view_file_item(file, tab));
            }
        }

        if !tab.untracked.is_empty() {
            content = content.push(
                row![
                    text("U N T R A C K E D").size(10).color(theme.overlay0()),
                    text(format!("{}", tab.untracked.len()))
                        .size(10)
                        .color(theme.text_secondary()),
                ]
                .spacing(6),
            );
            for file in &tab.untracked {
                content = content.push(self.view_file_item(file, tab));
            }
        }

        if tab.staged.is_empty() && tab.unstaged.is_empty() && tab.untracked.is_empty() {
            let msg = if show_loading {
                ""
            } else if tab.is_git_repo {
                "No changes"
            } else {
                "Not a git repository"
            };
            if !msg.is_empty() {
                content = content.push(text(msg).size(font).color(theme.text_secondary()));
            }
        }

        scrollable(content)
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
    }

    fn view_file_item<'a>(
        &'a self,
        file: &'a FileEntry,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let status_color = match file.status.as_str() {
            "A" => theme.success(),
            "M" => theme.warning(),
            "D" => theme.danger(),
            "R" => theme.accent(),
            _ => theme.text_secondary(),
        };

        let is_selected = tab.selected_file.as_ref() == Some(&file.path);
        let text_color = if is_selected {
            match self.theme {
                AppTheme::Dark => color!(0xffffff),
                AppTheme::Light => color!(0xffffff),
            }
        } else {
            theme.text_primary()
        };

        let font_small = self.ui_font_small();
        let file_row = row![
            text(&file.status)
                .size(font)
                .color(status_color)
                .width(Length::Fixed(20.0)),
            text(&file.path).size(font).color(text_color),
        ]
        .spacing(8);

        let btn_style = if is_selected {
            button::primary
        } else {
            button::text
        };

        let select_btn = button(file_row)
            .style(btn_style)
            .padding([4, 8])
            .width(Length::Fill)
            .on_press(Event::FileSelect(file.path.clone(), file.is_staged));

        // Don't show edit button for deleted files
        if file.status == "D" {
            return select_btn.into();
        }

        let full_path = tab.repo_path.join(&file.path);
        let edit_btn = button(
            text("\u{270e}")
                .size(font_small)
                .color(theme.text_secondary()),
        )
        .style(button::text)
        .padding([4, 6])
        .on_press(Event::EditFile(full_path));

        row![select_btn, edit_btn]
            .align_y(iced::Alignment::Center)
            .into()
    }

    fn view_diff_panel<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let font_small = self.ui_font_small();
        let mut content = Column::new().spacing(0);

        // Header
        let header_bg = theme.bg_overlay();
        let header = row![
            text(tab.selected_file.as_deref().unwrap_or(""))
                .size(font)
                .color(theme.text_primary()),
            iced::widget::Space::new().width(Length::Fill),
            text("j/k: navigate  Esc: back")
                .size(font_small)
                .color(theme.text_secondary()),
            iced::widget::Space::new().width(Length::Fixed(16.0)),
            button(text("Back to Terminal").size(font))
                .style(self.ghost_button_style())
                .padding([4, 12])
                .on_press(Event::ClearSelection),
        ]
        .padding(8)
        .spacing(8);

        content =
            content.push(
                container(header)
                    .width(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(header_bg.into()),
                        ..Default::default()
                    }),
            );

        // Diff content
        let mut diff_column = Column::new().spacing(0);
        let show_diff_loading_message = tab.diff_load_in_progress
            && tab.diff_load_started_at.is_some_and(|started| {
                started.elapsed() >= Duration::from_millis(LOADING_INDICATOR_DELAY_MS)
            });

        if tab.diff_load_in_progress {
            if show_diff_loading_message {
                diff_column = diff_column.push(
                    text("Loading diff...")
                        .size(font)
                        .color(theme.text_secondary()),
                );
            }
        } else if tab.diff_lines.is_empty() {
            diff_column = diff_column.push(
                text("No diff available")
                    .size(font)
                    .color(theme.text_secondary()),
            );
        } else {
            let total_lines = tab.diff_lines.len();
            let rendered_lines = total_lines.min(MAX_DIFF_VIEW_RENDER_LINES);
            if total_lines > rendered_lines {
                diff_column = diff_column.push(
                    container(
                        text(format!(
                            "Showing first {} of {} diff lines for performance.",
                            rendered_lines, total_lines
                        ))
                        .size(font_small)
                        .color(theme.warning()),
                    )
                    .width(Length::Fill)
                    .padding([6, 8])
                    .style(move |_| container::Style {
                        background: Some(theme.bg_overlay().into()),
                        border: iced::Border {
                            width: 1.0,
                            color: theme.surface0(),
                            radius: 0.0.into(),
                        },
                        ..Default::default()
                    }),
                );
            }

            for (idx, line) in tab.diff_lines.iter().take(rendered_lines).enumerate() {
                let syntax_segments = tab
                    .diff_syntax_lines
                    .as_ref()
                    .and_then(|lines| lines.get(idx))
                    .map(Vec::as_slice);
                diff_column = diff_column.push(self.view_diff_line(line, syntax_segments));
            }

            if total_lines > rendered_lines {
                diff_column = diff_column.push(
                    container(
                        text(format!(
                            "... {} additional diff lines not rendered",
                            total_lines.saturating_sub(rendered_lines)
                        ))
                        .size(font_small)
                        .color(theme.text_muted()),
                    )
                    .width(Length::Fill)
                    .padding([6, 8]),
                );
            }
        }

        content = content.push(
            scrollable(diff_column.padding(8))
                .height(Length::Fill)
                .width(Length::Fill),
        );

        let bg = theme.bg_base();
        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                ..Default::default()
            })
            .into()
    }

    fn view_diff_line<'a>(
        &'a self,
        line: &'a DiffLine,
        syntax_segments: Option<&'a [SyntaxHighlightSegment]>,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let (line_color, bg_color) = match line.line_type {
            DiffLineType::Addition => (theme.success(), Some(theme.diff_add_bg())),
            DiffLineType::Deletion => (theme.danger(), Some(theme.diff_del_bg())),
            DiffLineType::Header => (theme.accent(), None),
            DiffLineType::Context => (theme.text_secondary(), None),
        };

        // Line numbers
        let old_num = line
            .old_line_num
            .map(|n| format!("{:4}", n))
            .unwrap_or_else(|| "    ".to_string());
        let new_num = line
            .new_line_num
            .map(|n| format!("{:4}", n))
            .unwrap_or_else(|| "    ".to_string());

        let prefix = match line.line_type {
            DiffLineType::Addition => "+",
            DiffLineType::Deletion => "-",
            DiffLineType::Context => " ",
            DiffLineType::Header => "",
        };

        // Build content - either with inline changes or plain
        let content_element: Element<'a, Event, Theme, iced::Renderer> = if let Some(ref changes) =
            line.inline_changes
        {
            // Build rich text with word-level highlighting
            let mut content_row = Row::new().spacing(0);
            for change in changes {
                let (change_color, change_bg) = match (&line.line_type, &change.change_type) {
                    (DiffLineType::Deletion, ChangeType::Delete) => {
                        (color!(0xffffff), Some(theme.diff_del_highlight()))
                    }
                    (DiffLineType::Addition, ChangeType::Insert) => {
                        (color!(0xffffff), Some(theme.diff_add_highlight()))
                    }
                    _ => (line_color, None),
                };

                let change_text = text(&change.value)
                    .size(font)
                    .color(change_color)
                    .font(iced::Font::MONOSPACE);

                if let Some(bg) = change_bg {
                    content_row =
                        content_row.push(container(change_text).style(move |_| container::Style {
                            background: Some(bg.into()),
                            ..Default::default()
                        }));
                } else {
                    content_row = content_row.push(change_text);
                }
            }
            content_row.into()
        } else if let Some(segments) = syntax_segments {
            if segments.is_empty() {
                text(&line.content)
                    .size(font)
                    .color(line_color)
                    .font(iced::Font::MONOSPACE)
                    .into()
            } else {
                let mut content_row = Row::new().spacing(0);
                for segment in segments {
                    content_row = content_row.push(
                        text(segment.text.as_str())
                            .size(font)
                            .color(segment.color)
                            .font(iced::Font::MONOSPACE),
                    );
                }
                content_row.into()
            }
        } else {
            text(&line.content)
                .size(font)
                .color(line_color)
                .font(iced::Font::MONOSPACE)
                .into()
        };

        let line_num_color = theme.text_muted();
        let line_row = if line.line_type == DiffLineType::Header {
            row![content_element].spacing(0)
        } else {
            row![
                text(old_num)
                    .size(font)
                    .color(line_num_color)
                    .font(iced::Font::MONOSPACE),
                text(new_num)
                    .size(font)
                    .color(line_num_color)
                    .font(iced::Font::MONOSPACE),
                text(prefix)
                    .size(font)
                    .color(line_color)
                    .font(iced::Font::MONOSPACE),
                content_element,
            ]
            .spacing(4)
        };

        let line_container = container(line_row).width(Length::Fill).padding([1, 4]);

        if let Some(bg) = bg_color {
            line_container
                .style(move |_| container::Style {
                    background: Some(bg.into()),
                    ..Default::default()
                })
                .into()
        } else {
            line_container.into()
        }
    }

    /// Background placeholder for an agent tab. The wry webview is positioned
    /// by `calculate_webview_bounds` and overlays this area; the placeholder is
    /// only visible briefly during webview construction or as a fallback if
    /// the webview fails to attach.
    fn view_agent_tab<'a>(
        &'a self,
        _tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let bg = theme.bg_base();
        container(
            text("Loading agent…")
                .size(13)
                .color(theme.text_secondary()),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(move |_| container::Style {
            background: Some(bg.into()),
            ..Default::default()
        })
        .into()
    }

    fn view_terminal<'a>(&'a self, tab: &'a TabState) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;

        let bg = theme.bg_base();
        let terminal_view: Element<'a, Event, Theme, iced::Renderer> =
            if let Some(term) = tab.terminal() {
                let tab_id = tab.id;
                let term_container =
                    container(TerminalView::show(term).map(move |e| Event::Terminal(tab_id, e)))
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .padding(4)
                        .style(move |_| container::Style {
                            background: Some(bg.into()),
                            ..Default::default()
                        });
                iced::widget::mouse_area(term_container)
                    .on_press(Event::MainTerminalClicked)
                    .into()
            } else {
                container(
                    text("Terminal unavailable")
                        .size(14)
                        .color(theme.text_secondary()),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into()
            };

        // Stack search bar on top of terminal when active
        if tab.search.is_active {
            let search_bar = self.view_search_bar(tab);
            column![search_bar, terminal_view]
                .spacing(0)
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else {
            terminal_view
        }
    }

    fn view_bottom_panel(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let ws = match self.active_workspace() {
            Some(ws) => ws,
            None => {
                return iced::widget::Space::new().width(0).height(0).into();
            }
        };
        let console = &ws.console;
        let active_bottom_tab = ws.active_bottom_tab;

        // --- Tab bar ---
        let tab_bar = self.view_bottom_tab_bar(ws, console);

        if !self.console_expanded {
            let border_color = theme.surface0();
            return container(tab_bar)
                .width(Length::Fill)
                .height(Length::Fixed(CONSOLE_HEADER_HEIGHT))
                .style(move |_| container::Style {
                    background: None,
                    border: iced::Border {
                        width: 1.0,
                        color: border_color,
                        radius: 0.0.into(),
                    },
                    ..Default::default()
                })
                .into();
        }

        // --- Content area ---
        let content: Element<'_, Event, Theme, iced::Renderer> = match active_bottom_tab {
            BottomPanelTab::Console => self.view_console_output(console),
            BottomPanelTab::Terminal(idx) => {
                if let Some(bt) = ws.bottom_terminals.get(idx) {
                    if let Some(term) = &bt.terminal {
                        let bt_id = bt.id;
                        let bt_container = container(
                            TerminalView::show(term)
                                .map(move |e| Event::BottomTerminalEvent(bt_id, e)),
                        )
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .padding([2, 0]);
                        iced::widget::mouse_area(bt_container)
                            .on_press(Event::BottomTerminalClicked(idx))
                            .into()
                    } else {
                        let text_color = theme.text_secondary();
                        container(text("Terminal unavailable").size(14).color(text_color))
                            .width(Length::Fill)
                            .height(Length::Fill)
                            .center_x(Length::Fill)
                            .center_y(Length::Fill)
                            .into()
                    }
                } else {
                    // Invalid index — fall back to console
                    self.view_console_output(console)
                }
            }
        };

        let bg = theme.bg_crust();
        let main_panel = container(
            column![tab_bar, content]
                .spacing(0)
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fixed(self.console_height))
        .style(move |_| container::Style {
            background: Some(bg.into()),
            ..Default::default()
        });

        if self.quick_commands_visible {
            Stack::new()
                .push(main_panel)
                .push(self.view_quick_commands_picker())
                .width(Length::Fill)
                .height(Length::Fixed(self.console_height))
                .into()
        } else {
            main_panel.into()
        }
    }

    fn view_quick_commands_picker(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let bg = theme.bg_surface();
        let border_color = theme.border();
        let text_primary = theme.text_primary();
        let text_secondary = theme.text_secondary();
        let hover_bg = theme.surface0();
        let mono = iced::Font::with_name("Menlo");

        let mut items = Column::new().spacing(0).width(Length::Fixed(260.0));
        for (idx, qc) in self.quick_commands.iter().enumerate() {
            let hover = hover_bg;
            let name = qc.name.clone();
            let command = qc.command.clone();
            items = items.push(
                button(
                    row![
                        text("\u{25b8}")
                            .size(12)
                            .color(theme.peach())
                            .font(mono)
                            .width(Length::Fixed(20.0)),
                        column![
                            text(name).size(13).color(text_primary),
                            text(command).size(11).color(text_secondary).font(mono),
                        ]
                        .spacing(1)
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center)
                    .padding([6, 10]),
                )
                .style(move |_theme, status| {
                    let bg_color = if matches!(status, button::Status::Hovered) {
                        Some(hover.into())
                    } else {
                        None
                    };
                    button::Style {
                        background: bg_color,
                        text_color: text_primary,
                        border: iced::Border::default(),
                        ..Default::default()
                    }
                })
                .padding(0)
                .width(Length::Fill)
                .on_press(Event::RunQuickCommand(idx)),
            );
        }

        let picker_menu = container(items)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                border: iced::Border {
                    color: border_color,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                shadow: iced::Shadow {
                    color: iced::Color::from_rgba(0.0, 0.0, 0.0, 0.3),
                    offset: iced::Vector::new(0.0, -2.0),
                    blur_radius: 8.0,
                },
                ..Default::default()
            })
            .padding(4);

        // Click-away backdrop
        let backdrop = iced::widget::mouse_area(
            container(iced::widget::Space::new())
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .on_press(Event::HideQuickCommands);

        Stack::new()
            .push(backdrop)
            .push(
                container(picker_menu)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(iced::alignment::Horizontal::Left)
                    .align_y(iced::alignment::Vertical::Top)
                    .padding(iced::Padding {
                        top: 4.0,
                        right: 0.0,
                        bottom: 0.0,
                        left: 8.0,
                    }),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn view_bottom_tab_bar<'a>(
        &'a self,
        ws: &'a Workspace,
        console: &'a ConsoleState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let active_tab = ws.active_bottom_tab;

        // Chevron button (toggle expand/collapse)
        let chevron = if self.console_expanded {
            "\u{25BC}"
        } else {
            "\u{25B6}"
        };
        let chevron_color = theme.overlay0();
        let chevron_btn = button(text(chevron).size(10).color(chevron_color))
            .style(|_theme, _status| button::Style {
                background: Some(iced::Color::TRANSPARENT.into()),
                ..Default::default()
            })
            .padding([4, 6])
            .on_press(Event::ConsoleToggle);

        // --- Console tab button ---
        let console_is_active = active_tab == BottomPanelTab::Console;
        let dot_color = match console.status {
            ConsoleStatus::Running => theme.success(),
            ConsoleStatus::Error => theme.danger(),
            ConsoleStatus::Stopped | ConsoleStatus::NoneConfigured => theme.overlay0(),
        };
        let status_dot = container(iced::widget::Space::new())
            .width(Length::Fixed(6.0))
            .height(Length::Fixed(6.0))
            .style(move |_| container::Style {
                background: Some(dot_color.into()),
                border: iced::Border {
                    radius: 3.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            });

        let console_label_color = if console_is_active {
            theme.text_primary()
        } else {
            theme.overlay1()
        };
        let console_tab_bg = if console_is_active {
            theme.bg_overlay()
        } else {
            iced::Color::TRANSPARENT
        };
        let console_hover_bg = theme.surface0();
        let console_active_accent = if console_is_active {
            theme.accent()
        } else {
            iced::Color::TRANSPARENT
        };

        let console_tab_btn = button(
            row![
                status_dot,
                text("Console")
                    .size(12)
                    .color(console_label_color)
                    .font(iced::Font::with_name("Menlo"))
            ]
            .spacing(5)
            .align_y(iced::Alignment::Center),
        )
        .style(move |_theme, status| {
            let bg = if matches!(status, button::Status::Hovered) && !console_is_active {
                console_hover_bg
            } else {
                console_tab_bg
            };
            button::Style {
                background: Some(bg.into()),
                border: iced::Border {
                    width: 0.0,
                    color: iced::Color::TRANSPARENT,
                    radius: 3.0.into(),
                },
                text_color: console_label_color,
                ..Default::default()
            }
        })
        .padding([4, 10])
        .on_press(Event::BottomTabSelect(BottomPanelTab::Console));

        // Underline for console tab when active
        let console_tab_with_underline: Element<'a, Event, Theme, iced::Renderer> = column![
            console_tab_btn,
            container(iced::widget::Space::new())
                .width(Length::Fill)
                .height(Length::Fixed(2.0))
                .style(move |_| container::Style {
                    background: Some(console_active_accent.into()),
                    ..Default::default()
                })
        ]
        .spacing(0)
        .into();

        // --- Terminal tab buttons ---
        let mut tab_buttons: Vec<Element<'a, Event, Theme, iced::Renderer>> = Vec::new();
        for (idx, bt) in ws.bottom_terminals.iter().enumerate() {
            let is_active = active_tab == BottomPanelTab::Terminal(idx);
            let label: String = bt
                .title
                .clone()
                .unwrap_or_else(|| format!("Terminal {}", idx + 1));
            let label_color = if is_active {
                theme.text_primary()
            } else {
                theme.overlay1()
            };
            let tab_bg = if is_active {
                theme.bg_overlay()
            } else {
                iced::Color::TRANSPARENT
            };
            let tab_hover_bg = theme.surface0();
            let active_accent = if is_active {
                theme.accent()
            } else {
                iced::Color::TRANSPARENT
            };

            let close_color = theme.overlay0();
            let close_hover = theme.text_primary();
            let close_btn = button(text("\u{00D7}").size(12).color(close_color))
                .style(move |_theme, status| {
                    let c = if matches!(status, button::Status::Hovered) {
                        close_hover
                    } else {
                        close_color
                    };
                    button::Style {
                        background: Some(iced::Color::TRANSPARENT.into()),
                        text_color: c,
                        ..Default::default()
                    }
                })
                .padding([0, 2])
                .on_press(Event::BottomTerminalClose(idx));

            let tab_btn = button(
                row![
                    text(">_")
                        .size(10)
                        .color(label_color)
                        .font(iced::Font::with_name("Menlo")),
                    text(label)
                        .size(12)
                        .color(label_color)
                        .font(iced::Font::with_name("Menlo")),
                ]
                .spacing(4)
                .align_y(iced::Alignment::Center),
            )
            .style(move |_theme, status| {
                let bg = if matches!(status, button::Status::Hovered) && !is_active {
                    tab_hover_bg
                } else {
                    tab_bg
                };
                button::Style {
                    background: Some(bg.into()),
                    border: iced::Border {
                        radius: 3.0.into(),
                        ..Default::default()
                    },
                    text_color: label_color,
                    ..Default::default()
                }
            })
            .padding([4, 8])
            .on_press(Event::BottomTabSelect(BottomPanelTab::Terminal(idx)));

            let tab_with_close: Element<'a, Event, Theme, iced::Renderer> = column![
                row![tab_btn, close_btn]
                    .spacing(0)
                    .align_y(iced::Alignment::Center),
                container(iced::widget::Space::new())
                    .width(Length::Fill)
                    .height(Length::Fixed(2.0))
                    .style(move |_| container::Style {
                        background: Some(active_accent.into()),
                        ..Default::default()
                    })
            ]
            .spacing(0)
            .into();

            tab_buttons.push(tab_with_close);
        }

        // "+" button to add terminal
        let plus_color = theme.overlay1();
        let plus_hover_bg = theme.surface0();
        let plus_btn = button(text("+").size(14).color(plus_color))
            .style(move |_theme, status| {
                let bg = if matches!(status, button::Status::Hovered) {
                    plus_hover_bg
                } else {
                    iced::Color::TRANSPARENT
                };
                button::Style {
                    background: Some(bg.into()),
                    border: iced::Border {
                        radius: 4.0.into(),
                        ..Default::default()
                    },
                    text_color: plus_color,
                    ..Default::default()
                }
            })
            .padding([2, 8])
            .on_press(Event::BottomTerminalAdd);

        // Spacer
        let spacer = iced::widget::Space::new().width(Length::Fill);

        // --- Contextual controls (right side, only when Console tab is active) ---
        let mut header_row = Row::new()
            .spacing(4)
            .align_y(iced::Alignment::Center)
            .padding([0, 8])
            .push(chevron_btn)
            .push(console_tab_with_underline);

        for tb in tab_buttons {
            header_row = header_row.push(tb);
        }
        header_row = header_row.push(plus_btn);

        // Quick commands button (⚡) — only show if commands are configured
        if !self.quick_commands.is_empty() {
            let qc_color = theme.peach();
            let qc_hover_bg = theme.surface0();
            let qc_btn = button(text("\u{26a1}").size(12).color(qc_color))
                .style(move |_theme, status| {
                    let bg = if matches!(status, button::Status::Hovered) {
                        qc_hover_bg
                    } else {
                        iced::Color::TRANSPARENT
                    };
                    button::Style {
                        background: Some(bg.into()),
                        border: iced::Border {
                            radius: 4.0.into(),
                            ..Default::default()
                        },
                        text_color: qc_color,
                        ..Default::default()
                    }
                })
                .padding([2, 6])
                .on_press(Event::ShowQuickCommands);
            header_row = header_row.push(qc_btn);
        }

        header_row = header_row.push(spacer);

        // Console-specific controls on the right
        if console_is_active {
            // Process name — click to edit, or show text input when editing
            let name_element: Element<'a, Event, Theme, iced::Renderer> =
                if let Some(edit_val) = &self.editing_console_command {
                    let input_bg = theme.bg_base();
                    let input_border = theme.accent();
                    text_input("e.g. cargo run, bun run dev", edit_val)
                        .on_input(Event::ConsoleCommandChanged)
                        .on_submit(Event::ConsoleCommandSubmit)
                        .size(12)
                        .width(Length::Fixed(220.0))
                        .padding([3, 6])
                        .style(move |_theme, _status| text_input::Style {
                            background: input_bg.into(),
                            border: iced::Border {
                                width: 1.0,
                                color: input_border,
                                radius: 3.0.into(),
                            },
                            icon: iced::Color::TRANSPARENT,
                            placeholder: theme.overlay0(),
                            value: theme.text_primary(),
                            selection: theme.accent(),
                        })
                        .into()
                } else {
                    let process_name = console
                        .run_command
                        .as_deref()
                        .unwrap_or("Click to set command");
                    let name_color = if console.run_command.is_some() {
                        theme.text_primary()
                    } else {
                        theme.overlay0()
                    };
                    let hover_bg = theme.surface0();
                    button(
                        text(process_name)
                            .size(12)
                            .color(name_color)
                            .font(iced::Font::with_name("Menlo")),
                    )
                    .style(move |_theme, status| {
                        let bg = if matches!(status, button::Status::Hovered) {
                            hover_bg
                        } else {
                            iced::Color::TRANSPARENT
                        };
                        button::Style {
                            background: Some(bg.into()),
                            border: iced::Border {
                                radius: 3.0.into(),
                                ..Default::default()
                            },
                            text_color: name_color,
                            ..Default::default()
                        }
                    })
                    .padding([2, 4])
                    .on_press(Event::ConsoleCommandEditStart)
                    .into()
                };

            let uptime = console.uptime_string();
            let uptime_label = text(uptime)
                .size(11)
                .color(theme.overlay0())
                .font(iced::Font::with_name("Menlo"));

            let btn_color = theme.overlay1();
            let hover_bg = theme.surface0();
            let action_btn_style = move |_theme: &Theme, status: button::Status| {
                let bg = if matches!(status, button::Status::Hovered) {
                    hover_bg
                } else {
                    iced::Color::TRANSPARENT
                };
                button::Style {
                    background: Some(bg.into()),
                    border: iced::Border {
                        radius: 4.0.into(),
                        ..Default::default()
                    },
                    text_color: btn_color,
                    ..Default::default()
                }
            };

            let browser_btn: Option<Element<'a, Event, Theme, iced::Renderer>> =
                if console.detected_url.is_some() {
                    let link_color = theme.accent();
                    let hover_bg_browser = theme.surface0();
                    Some(
                        button(text("\u{1F517}").size(12).color(link_color))
                            .style(move |_theme, status| {
                                let bg = if matches!(status, button::Status::Hovered) {
                                    hover_bg_browser
                                } else {
                                    iced::Color::TRANSPARENT
                                };
                                button::Style {
                                    background: Some(bg.into()),
                                    border: iced::Border {
                                        radius: 4.0.into(),
                                        ..Default::default()
                                    },
                                    text_color: link_color,
                                    ..Default::default()
                                }
                            })
                            .padding([2, 6])
                            .on_press(Event::ConsoleOpenBrowser)
                            .into(),
                    )
                } else {
                    None
                };

            let clear_btn = button(text("\u{2300}").size(12).color(btn_color))
                .style(action_btn_style)
                .padding([2, 6])
                .on_press(Event::ConsoleClearOutput);

            let restart_btn = button(text("\u{21BB}").size(12).color(btn_color))
                .style(action_btn_style)
                .padding([2, 6])
                .on_press(Event::ConsoleRestart);

            let stop_start_btn = if console.is_running() {
                let stop_color = theme.danger();
                button(text("\u{25A0}").size(12).color(stop_color))
                    .style(move |_theme, status| {
                        let bg = if matches!(status, button::Status::Hovered) {
                            hover_bg
                        } else {
                            iced::Color::TRANSPARENT
                        };
                        button::Style {
                            background: Some(bg.into()),
                            border: iced::Border {
                                radius: 4.0.into(),
                                ..Default::default()
                            },
                            text_color: stop_color,
                            ..Default::default()
                        }
                    })
                    .padding([2, 6])
                    .on_press(Event::ConsoleStop)
            } else {
                let start_color = theme.success();
                button(text("\u{25B6}").size(12).color(start_color))
                    .style(move |_theme, status| {
                        let bg = if matches!(status, button::Status::Hovered) {
                            hover_bg
                        } else {
                            iced::Color::TRANSPARENT
                        };
                        button::Style {
                            background: Some(bg.into()),
                            border: iced::Border {
                                radius: 4.0.into(),
                                ..Default::default()
                            },
                            text_color: start_color,
                            ..Default::default()
                        }
                    })
                    .padding([2, 6])
                    .on_press_maybe(if console.run_command.is_some() {
                        Some(Event::ConsoleStart)
                    } else {
                        None
                    })
            };

            let search_icon_color = if console.search_visible {
                theme.accent()
            } else {
                btn_color
            };
            let log_server_status = if self.log_server_enabled {
                if self.log_server_state.base_url().is_some() {
                    ("Logs:on", theme.success())
                } else {
                    ("Logs:...", theme.warning())
                }
            } else {
                ("Logs:off", theme.overlay0())
            };
            let log_toggle_btn = button(
                text(log_server_status.0)
                    .size(11)
                    .color(log_server_status.1)
                    .font(iced::Font::with_name("Menlo")),
            )
            .style(action_btn_style)
            .padding([2, 6])
            .on_press(Event::ToggleLogServer);
            let search_btn = button(text("\u{2315}").size(12).color(search_icon_color))
                .style(action_btn_style)
                .padding([2, 6])
                .on_press(Event::ConsoleSearchToggle);

            header_row = header_row.push(name_element).push(uptime_label);
            if let Some(btn) = browser_btn {
                header_row = header_row.push(btn);
            }
            header_row = header_row
                .push(log_toggle_btn)
                .push(search_btn)
                .push(clear_btn)
                .push(restart_btn)
                .push(stop_start_btn);
        }

        let header_bg = theme.bg_surface();
        let top_border = theme.surface0();

        container(header_row)
            .width(Length::Fill)
            .height(Length::Fixed(CONSOLE_HEADER_HEIGHT))
            .center_y(Length::Fixed(CONSOLE_HEADER_HEIGHT))
            .style(move |_| container::Style {
                background: Some(header_bg.into()),
                border: iced::Border {
                    width: 1.0,
                    color: top_border,
                    radius: 0.0.into(),
                },
                ..Default::default()
            })
            .into()
    }

    fn view_console_output<'a>(
        &'a self,
        console: &'a ConsoleState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;

        if console.output_lines.is_empty() {
            // Show hint text
            let hint = if console.run_command.is_none() {
                "No command configured for this workspace"
            } else if console.status == ConsoleStatus::Stopped
                || console.status == ConsoleStatus::NoneConfigured
            {
                "Press \u{25B6} to start"
            } else {
                "Waiting for output..."
            };

            let bg = theme.bg_crust();
            return container(
                text(hint)
                    .size(13)
                    .color(theme.overlay0())
                    .font(iced::Font::with_name("Menlo")),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                ..Default::default()
            })
            .into();
        }

        let bg = theme.bg_crust();
        let text_color = theme.text_secondary();
        let selection_color = theme.surface2();

        let editor: Element<'_, Event, Theme, iced::Renderer> = container(
            text_editor(&console.editor_content)
                .on_action(Event::ConsoleEditorAction)
                .font(iced::Font::with_name("Menlo"))
                .size(13)
                .padding([4, 8])
                .style(move |_theme, _status| text_editor::Style {
                    background: bg.into(),
                    border: iced::Border::default(),
                    placeholder: text_color,
                    value: text_color,
                    selection: selection_color,
                })
                .height(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .into();

        if console.search_visible {
            let search_bar = self.view_console_search_bar(console);
            column![search_bar, editor]
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else {
            editor
        }
    }

    fn view_console_search_bar<'a>(
        &'a self,
        console: &'a ConsoleState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();

        let match_display = if console.search_query.is_empty() {
            String::new()
        } else {
            let count = console.matching_line_count();
            if count == 0 {
                "No matches".to_string()
            } else {
                format!("{} matching lines", count)
            }
        };

        let search_input = text_input("Filter output...", &console.search_query)
            .on_input(Event::ConsoleSearchChanged)
            .size(font)
            .width(Length::Fixed(200.0))
            .padding([4, 8]);

        let match_text_color =
            if !console.search_query.is_empty() && console.matching_line_count() == 0 {
                theme.danger()
            } else {
                theme.overlay1()
            };

        let match_label = text(match_display).size(font).color(match_text_color);

        let close_color = theme.overlay1();
        let hover_bg = theme.surface0();
        let close_btn = button(text("\u{2715}").size(12).color(close_color))
            .style(move |_theme, status| {
                let bg = if matches!(status, button::Status::Hovered) {
                    hover_bg
                } else {
                    iced::Color::TRANSPARENT
                };
                button::Style {
                    background: Some(bg.into()),
                    border: iced::Border {
                        radius: 4.0.into(),
                        ..Default::default()
                    },
                    text_color: close_color,
                    ..Default::default()
                }
            })
            .padding([2, 6])
            .on_press(Event::ConsoleSearchClose);

        let bar_bg = theme.bg_surface();
        let border_color = theme.surface0();

        container(
            row![search_input, match_label, close_btn]
                .spacing(8)
                .align_y(iced::Alignment::Center)
                .padding([4, 8]),
        )
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(bar_bg.into()),
            border: iced::Border {
                width: 1.0,
                color: border_color,
                radius: 0.0.into(),
            },
            ..Default::default()
        })
        .into()
    }

    fn view_search_bar<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let font_small = self.ui_font_small();

        // Match count display
        let match_display = if tab.search.matches.is_empty() {
            if tab.search.query.is_empty() {
                String::new()
            } else {
                "No matches".to_string()
            }
        } else {
            format!(
                "{}/{}",
                tab.search.current_match + 1,
                tab.search.matches.len()
            )
        };

        let has_matches = !tab.search.matches.is_empty();

        let search_input = text_input("Search...", &tab.search.query)
            .on_input(Event::SearchQueryChanged)
            .on_submit(Event::SearchExecute)
            .size(font)
            .width(Length::Fixed(200.0))
            .padding([4, 8]);

        let prev_btn = button(text("<").size(font))
            .style(if has_matches {
                button::secondary
            } else {
                button::text
            })
            .padding([4, 8])
            .on_press_maybe(if has_matches {
                Some(Event::SearchPrev)
            } else {
                None
            });

        let next_btn = button(text(">").size(font))
            .style(if has_matches {
                button::secondary
            } else {
                button::text
            })
            .padding([4, 8])
            .on_press_maybe(if has_matches {
                Some(Event::SearchNext)
            } else {
                None
            });

        let close_btn = button(text("x").size(font))
            .style(button::text)
            .padding([4, 8])
            .on_press(Event::SearchClose);

        let bar_bg = theme.bg_overlay();
        container(
            row![
                search_input,
                text(match_display)
                    .size(font_small)
                    .color(theme.text_secondary()),
                prev_btn,
                next_btn,
                iced::widget::Space::new().width(Length::Fill),
                text("Esc: close  Cmd+G: next  Cmd+Shift+G: prev")
                    .size(font_small)
                    .color(theme.text_muted()),
                close_btn,
            ]
            .spacing(8)
            .padding(8)
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(bar_bg.into()),
            ..Default::default()
        })
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // === ConsoleState::strip_ansi ===

    #[test]
    fn strip_ansi_plain_text() {
        assert_eq!(ConsoleState::strip_ansi("hello world"), "hello world");
    }

    #[test]
    fn strip_ansi_color_codes() {
        assert_eq!(
            ConsoleState::strip_ansi("\x1b[31mred text\x1b[0m"),
            "red text"
        );
    }

    #[test]
    fn strip_ansi_bold() {
        assert_eq!(ConsoleState::strip_ansi("\x1b[1mbold\x1b[0m"), "bold");
    }

    #[test]
    fn strip_ansi_nested() {
        assert_eq!(
            ConsoleState::strip_ansi("\x1b[1m\x1b[31mbold red\x1b[0m\x1b[0m"),
            "bold red"
        );
    }

    #[test]
    fn strip_ansi_empty() {
        assert_eq!(ConsoleState::strip_ansi(""), "");
    }

    // === ConsoleState::detect_url ===

    #[test]
    fn detect_url_http() {
        assert_eq!(
            ConsoleState::detect_url("Server at http://localhost:3000"),
            Some("http://localhost:3000".to_string())
        );
    }

    #[test]
    fn detect_url_https_localhost() {
        assert_eq!(
            ConsoleState::detect_url("Running on https://localhost:8443/api"),
            Some("https://localhost:8443/api".to_string())
        );
    }

    #[test]
    fn detect_url_port_pattern() {
        assert_eq!(
            ConsoleState::detect_url("listening on :4000"),
            Some("http://localhost:4000".to_string())
        );
    }

    #[test]
    fn detect_url_on_port() {
        assert_eq!(
            ConsoleState::detect_url("Server started on port 8080"),
            Some("http://localhost:8080".to_string())
        );
    }

    #[test]
    fn detect_url_no_match() {
        assert_eq!(ConsoleState::detect_url("just some log output"), None);
    }

    #[test]
    fn detect_url_in_ansi() {
        assert_eq!(
            ConsoleState::detect_url("\x1b[32mhttp://localhost:3000\x1b[0m"),
            Some("http://localhost:3000".to_string())
        );
    }

    #[test]
    fn detect_url_trailing_punctuation() {
        // Trailing quotes/parens should be trimmed from the URL
        assert_eq!(
            ConsoleState::detect_url("Visit http://localhost:5000)"),
            Some("http://localhost:5000".to_string())
        );
    }

    #[test]
    fn detect_url_http_ip() {
        assert_eq!(
            ConsoleState::detect_url("http://127.0.0.1:9090/health"),
            Some("http://127.0.0.1:9090/health".to_string())
        );
    }

    // === compute_word_diff ===

    #[test]
    fn word_diff_identical() {
        let changes = compute_word_diff("hello world", "hello world");
        assert!(changes.iter().all(|c| c.change_type == ChangeType::Equal));
    }

    #[test]
    fn word_diff_insertion() {
        let changes = compute_word_diff("hello", "hello world");
        assert!(changes.iter().any(|c| c.change_type == ChangeType::Insert));
    }

    #[test]
    fn word_diff_mixed() {
        let changes = compute_word_diff("foo bar", "foo baz");
        assert!(changes.iter().any(|c| c.change_type == ChangeType::Delete));
        assert!(changes.iter().any(|c| c.change_type == ChangeType::Insert));
    }

    // === status_char ===

    #[test]
    fn status_char_staged_new() {
        assert_eq!(status_char(Status::INDEX_NEW, true), "A");
    }

    #[test]
    fn status_char_staged_modified() {
        assert_eq!(status_char(Status::INDEX_MODIFIED, true), "M");
    }

    #[test]
    fn status_char_staged_deleted() {
        assert_eq!(status_char(Status::INDEX_DELETED, true), "D");
    }

    #[test]
    fn status_char_staged_renamed() {
        assert_eq!(status_char(Status::INDEX_RENAMED, true), "R");
    }

    #[test]
    fn status_char_unstaged_modified() {
        assert_eq!(status_char(Status::WT_MODIFIED, false), "M");
    }

    #[test]
    fn status_char_unstaged_deleted() {
        assert_eq!(status_char(Status::WT_DELETED, false), "D");
    }

    // === TabState::is_image_file / is_markdown_file ===

    #[test]
    fn is_image_png() {
        assert!(TabState::is_image_file(Path::new("photo.png")));
    }

    #[test]
    fn is_image_txt() {
        assert!(!TabState::is_image_file(Path::new("readme.txt")));
    }

    #[test]
    fn is_image_no_extension() {
        assert!(!TabState::is_image_file(Path::new("Makefile")));
    }

    #[test]
    fn is_image_case_insensitive() {
        assert!(TabState::is_image_file(Path::new("photo.PNG")));
        assert!(TabState::is_image_file(Path::new("photo.JpEg")));
    }

    #[test]
    fn is_markdown_md() {
        assert!(TabState::is_markdown_file(Path::new("README.md")));
    }

    #[test]
    fn is_markdown_markdown_ext() {
        assert!(TabState::is_markdown_file(Path::new("doc.markdown")));
    }

    #[test]
    fn is_markdown_rs() {
        assert!(!TabState::is_markdown_file(Path::new("main.rs")));
    }

    // === detect_run_command ===

    #[test]
    fn detect_run_command_cargo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();
        assert_eq!(
            detect_run_command(&dir.path().to_path_buf()),
            Some("cargo run".to_string())
        );
    }

    #[test]
    fn detect_run_command_npm_dev() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"dev":"vite"}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_run_command(&dir.path().to_path_buf()),
            Some("npm run dev".to_string())
        );
    }

    #[test]
    fn detect_run_command_docker_compose() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("docker-compose.yml"), "version: '3'").unwrap();
        assert_eq!(
            detect_run_command(&dir.path().to_path_buf()),
            Some("docker compose up".to_string())
        );
    }

    #[test]
    fn detect_run_command_go() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module test").unwrap();
        std::fs::write(dir.path().join("main.go"), "package main").unwrap();
        assert_eq!(
            detect_run_command(&dir.path().to_path_buf()),
            Some("go run .".to_string())
        );
    }

    #[test]
    fn detect_run_command_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect_run_command(&dir.path().to_path_buf()), None);
    }

    // === Workspace::derive_abbrev ===

    #[test]
    fn derive_abbrev_normal() {
        assert_eq!(Workspace::derive_abbrev("gitterm"), "GI");
    }

    #[test]
    fn derive_abbrev_single_char() {
        assert_eq!(Workspace::derive_abbrev("x"), "X");
    }

    #[test]
    fn derive_abbrev_empty() {
        assert_eq!(Workspace::derive_abbrev(""), "");
    }

    #[test]
    fn derive_abbrev_unicode() {
        assert_eq!(Workspace::derive_abbrev("über"), "ÜB");
    }

    // === WorkspaceColor ===

    #[test]
    fn workspace_color_from_index_zero() {
        assert_eq!(WorkspaceColor::from_index(0), WorkspaceColor::Lavender);
    }

    #[test]
    fn workspace_color_from_index_seven() {
        assert_eq!(WorkspaceColor::from_index(7), WorkspaceColor::Teal);
    }

    #[test]
    fn workspace_color_from_index_wraps() {
        assert_eq!(WorkspaceColor::from_index(8), WorkspaceColor::Lavender);
    }

    #[test]
    fn workspace_color_next_available_none_used() {
        assert_eq!(
            WorkspaceColor::next_available(&[]),
            WorkspaceColor::Lavender
        );
    }

    #[test]
    fn workspace_color_next_available_some_used() {
        assert_eq!(
            WorkspaceColor::next_available(&[WorkspaceColor::Lavender, WorkspaceColor::Blue]),
            WorkspaceColor::Green
        );
    }

    #[test]
    fn workspace_color_next_available_all_used() {
        let all = WorkspaceColor::ALL.to_vec();
        // When all are used, wraps around
        let result = WorkspaceColor::next_available(&all);
        assert_eq!(result, WorkspaceColor::from_index(all.len()));
    }

    // === AppTheme::toggle ===

    #[test]
    fn theme_toggle_dark_to_light() {
        assert_eq!(AppTheme::Dark.toggle(), AppTheme::Light);
    }

    #[test]
    fn theme_toggle_light_to_dark() {
        assert_eq!(AppTheme::Light.toggle(), AppTheme::Dark);
    }

    // === Additional detect_url edge cases ===

    #[test]
    fn detect_url_port_zero() {
        // Port 0 should not match (guard: port > 0)
        assert_eq!(ConsoleState::detect_url("listening on :0"), None);
    }

    #[test]
    fn detect_url_no_digits_after_port() {
        assert_eq!(ConsoleState::detect_url("listening on :abc"), None);
    }

    #[test]
    fn detect_url_http_with_path_and_query() {
        assert_eq!(
            ConsoleState::detect_url("http://localhost:3000/api?key=val"),
            Some("http://localhost:3000/api?key=val".to_string())
        );
    }

    #[test]
    fn detect_url_quoted_url() {
        // Double-quote should terminate the URL
        assert_eq!(
            ConsoleState::detect_url(r#"Visit "http://localhost:8080/app" now"#),
            Some("http://localhost:8080/app".to_string())
        );
    }

    #[test]
    fn detect_url_256_color_ansi() {
        // 256-color ANSI: ESC[38;5;82m
        assert_eq!(
            ConsoleState::detect_url("\x1b[38;5;82mhttp://localhost:4000\x1b[0m"),
            Some("http://localhost:4000".to_string())
        );
    }

    // === Additional detect_run_command cases ===

    #[test]
    fn detect_run_command_npm_start() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"start":"node server.js"}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_run_command(&dir.path().to_path_buf()),
            Some("npm start".to_string())
        );
    }

    #[test]
    fn detect_run_command_bun_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"dev":"vite"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("bun.lockb"), "").unwrap();
        assert_eq!(
            detect_run_command(&dir.path().to_path_buf()),
            Some("bun run dev".to_string())
        );
    }

    #[test]
    fn detect_run_command_yarn_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"dev":"next dev"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("yarn.lock"), "").unwrap();
        assert_eq!(
            detect_run_command(&dir.path().to_path_buf()),
            Some("yarn run dev".to_string())
        );
    }

    #[test]
    fn detect_run_command_pnpm_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"dev":"nuxt dev"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(
            detect_run_command(&dir.path().to_path_buf()),
            Some("pnpm run dev".to_string())
        );
    }

    #[test]
    fn detect_run_command_tauri_with_cargo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src-tauri")).unwrap();
        std::fs::write(
            dir.path().join("src-tauri").join("Cargo.toml"),
            "[package]\nname = \"app\"",
        )
        .unwrap();
        // No package.json — falls back to cargo tauri dev
        assert_eq!(
            detect_run_command(&dir.path().to_path_buf()),
            Some("cd src-tauri && cargo tauri dev".to_string())
        );
    }

    #[test]
    fn detect_run_command_tauri_with_package_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src-tauri")).unwrap();
        std::fs::write(
            dir.path().join("src-tauri").join("Cargo.toml"),
            "[package]\nname = \"app\"",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"tauri":"tauri dev"}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_run_command(&dir.path().to_path_buf()),
            Some("npm run tauri".to_string())
        );
    }

    #[test]
    fn detect_run_command_go_with_cmd_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module test").unwrap();
        std::fs::create_dir(dir.path().join("cmd")).unwrap();
        // cmd/ dir is an alternative to main.go
        assert_eq!(
            detect_run_command(&dir.path().to_path_buf()),
            Some("go run .".to_string())
        );
    }

    #[test]
    fn detect_run_command_docker_compose_yaml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("docker-compose.yaml"), "version: '3'").unwrap();
        assert_eq!(
            detect_run_command(&dir.path().to_path_buf()),
            Some("docker compose up".to_string())
        );
    }

    #[test]
    fn detect_run_command_package_json_before_cargo() {
        // When both package.json and Cargo.toml exist (hybrid project),
        // package.json takes priority
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"dev":"vite"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(
            detect_run_command(&dir.path().to_path_buf()),
            Some("npm run dev".to_string())
        );
    }

    // === collect_file_tree ===

    #[test]
    fn collect_file_tree_sorts_dirs_before_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("zebra.txt"), "").unwrap();
        std::fs::write(dir.path().join("apple.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join("beta_dir")).unwrap();
        std::fs::create_dir(dir.path().join("alpha_dir")).unwrap();

        let snapshot = collect_file_tree(1, dir.path().to_path_buf(), false);
        let names: Vec<&str> = snapshot.entries.iter().map(|e| e.name.as_str()).collect();
        // Dirs first (sorted), then files (sorted)
        assert_eq!(
            names,
            vec!["alpha_dir", "beta_dir", "apple.txt", "zebra.txt"]
        );
    }

    #[test]
    fn collect_file_tree_hides_dotfiles() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".hidden"), "").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "").unwrap();

        let snapshot = collect_file_tree(1, dir.path().to_path_buf(), false);
        let names: Vec<&str> = snapshot.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["visible.txt"]);
    }

    #[test]
    fn collect_file_tree_shows_dotfiles_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".hidden"), "").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "").unwrap();

        let snapshot = collect_file_tree(1, dir.path().to_path_buf(), true);
        let names: Vec<&str> = snapshot.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&".hidden"));
        assert!(names.contains(&"visible.txt"));
    }

    #[test]
    fn collect_file_tree_excludes_node_modules_and_target() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::create_dir(dir.path().join("target")).unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();

        let snapshot = collect_file_tree(1, dir.path().to_path_buf(), false);
        let names: Vec<&str> = snapshot.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["src"]);
    }

    #[test]
    fn collect_file_tree_case_insensitive_sort() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Zebra.txt"), "").unwrap();
        std::fs::write(dir.path().join("apple.txt"), "").unwrap();
        std::fs::write(dir.path().join("Banana.txt"), "").unwrap();

        let snapshot = collect_file_tree(1, dir.path().to_path_buf(), false);
        let names: Vec<&str> = snapshot.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["apple.txt", "Banana.txt", "Zebra.txt"]);
    }

    #[test]
    fn collect_file_tree_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = collect_file_tree(1, dir.path().to_path_buf(), false);
        assert!(snapshot.entries.is_empty());
    }

    #[test]
    fn read_text_preview_limits_lines() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("large.txt");
        std::fs::write(&file, "a\nb\nc\nd\ne\n").unwrap();
        let preview = read_text_preview(&file, 1024, 3).unwrap();
        assert_eq!(preview, "a\nb\nc");
    }

    #[test]
    fn format_bytes_human_readable() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MB");
    }

    #[test]
    fn syntect_detects_typescript_extensions() {
        let ts = syntect_syntax_for_path(Path::new("example.ts"));
        let tsx = syntect_syntax_for_path(Path::new("example.tsx"));

        assert_ne!(ts.name, "Plain Text");
        assert_ne!(tsx.name, "Plain Text");
    }

    // === add_word_diffs_to_lines ===

    #[test]
    fn add_word_diffs_pairs_deletion_and_addition() {
        let mut lines = vec![
            DiffLine {
                content: "old text here".to_string(),
                line_type: DiffLineType::Deletion,
                old_line_num: Some(1),
                new_line_num: None,
                inline_changes: None,
            },
            DiffLine {
                content: "new text here".to_string(),
                line_type: DiffLineType::Addition,
                old_line_num: None,
                new_line_num: Some(1),
                inline_changes: None,
            },
        ];
        add_word_diffs_to_lines(&mut lines);
        // Both lines should have inline_changes populated
        assert!(lines[0].inline_changes.is_some());
        assert!(lines[1].inline_changes.is_some());
        // Deletion line should contain Delete changes
        let del_changes = lines[0].inline_changes.as_ref().unwrap();
        assert!(del_changes
            .iter()
            .any(|c| c.change_type == ChangeType::Delete));
        // Addition line should contain Insert changes
        let add_changes = lines[1].inline_changes.as_ref().unwrap();
        assert!(add_changes
            .iter()
            .any(|c| c.change_type == ChangeType::Insert));
    }

    #[test]
    fn add_word_diffs_no_pairs() {
        // Context lines should not get inline changes
        let mut lines = vec![DiffLine {
            content: "context".to_string(),
            line_type: DiffLineType::Context,
            old_line_num: Some(1),
            new_line_num: Some(1),
            inline_changes: None,
        }];
        add_word_diffs_to_lines(&mut lines);
        assert!(lines[0].inline_changes.is_none());
    }

    #[test]
    fn add_word_diffs_completely_different_lines() {
        // When lines are completely different (no Equal parts), inline_changes
        // should NOT be set (the function only sets them when there's at least one Equal)
        let mut lines = vec![
            DiffLine {
                content: "aaa".to_string(),
                line_type: DiffLineType::Deletion,
                old_line_num: Some(1),
                new_line_num: None,
                inline_changes: None,
            },
            DiffLine {
                content: "bbb".to_string(),
                line_type: DiffLineType::Addition,
                old_line_num: None,
                new_line_num: Some(1),
                inline_changes: None,
            },
        ];
        add_word_diffs_to_lines(&mut lines);
        assert!(lines[0].inline_changes.is_none());
        assert!(lines[1].inline_changes.is_none());
    }

    // === strip_ansi additional edge cases ===

    #[test]
    fn strip_ansi_bare_escape() {
        // Lone ESC without [ should skip ESC + next char
        assert_eq!(ConsoleState::strip_ansi("\x1bXhello"), "hello");
    }

    #[test]
    fn strip_ansi_preserves_non_escape_content() {
        let input = "line1\nline2\ttab";
        assert_eq!(ConsoleState::strip_ansi(input), input);
    }

    #[cfg(feature = "stt")]
    #[test]
    fn stt_audio_stats_empty() {
        assert_eq!(stt_audio_stats(&[]), (0.0, 0.0));
    }

    #[cfg(feature = "stt")]
    #[test]
    fn stt_audio_stats_reports_rms_and_peak() {
        let (rms, peak) = stt_audio_stats(&[0.0, 0.5, -0.5]);
        let expected_rms = (0.5f32 / 3.0).sqrt();
        assert!((rms - expected_rms).abs() < 0.000001);
        assert_eq!(peak, 0.5);
    }
}
