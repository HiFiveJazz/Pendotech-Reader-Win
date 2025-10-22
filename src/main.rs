use anyhow::{bail, Context, Result};
use chrono::Local;
use eframe::egui;
use egui_extras::StripBuilder;
use egui_plot::{Legend, Line, Plot, PlotBounds, PlotPoints, VLine};
use libloading::{Library, Symbol};
use serialport::{SerialPortInfo, SerialPortType};
use std::collections::VecDeque;
use std::ffi::{c_double, c_void, CStr, CString};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::thread;
use std::time::{Duration, Instant};

//
// ========================== CONFIG ==========================
//

// --- cDAQ ---
const CDAQ_S_RATE: f64 = 200.0;
// If you are currently getting ~4 Hz cDAQ samples, keep S_PER_READ = 50 (200/50 = 4).
// We will write to CSV only on every other cDAQ tick to achieve 2 Hz in the CSV.
const CDAQ_S_PER_READ: u32 = 50;
const CDAQ_READ_TIMEOUT: f64 = 5.0;
const CDAQ_BUFFER_SEC: f64 = 300.0;
const CDAQ_PREFERRED_MOD: u32 = 1;
const CDAQ_AI_INDICES: &[u32] = &[0, 2, 4, 6];

// --- PendoTech ---
const PT_BAUD: u32 = 9600;
const PT_READ_TIMEOUT: Duration = Duration::from_millis(50);
const PT_FRAME_DEADLINE: Duration = Duration::from_millis(700);
const PT_GUARD_AFTER_GOOD_FRAME: Duration = Duration::from_millis(15);
const PT_REQUEST: &[u8] = b"AZK\r\n";
const DLE: u8 = 0x10;
const STX: u8 = 0x02;
const ETX: u8 = 0x03;

// Windows
const LIVE_WINDOW_SECS: f64 = 30.0;
const FULL_WINDOW_SECS: f64 = 12.0 * 3600.0; // 12 hours

// Fixed Y ranges
const Y_CDAQ_MIN: f64 = -10.0;
const Y_CDAQ_MAX: f64 = 110.0;
const Y_PENDO_MIN: f64 = -10.0;
const Y_PENDO_MAX: f64 = 60.0;

// CSV destination base folder
const CSV_BASE: &str =
    r"C:\Users\Lab 2\OneDrive - Somatek Inc\Desktop\A to D Data";

//
// ====================== NI-DAQmx FFI ========================
//
#[cfg(target_pointer_width = "64")]
type TaskHandle = u64;
#[cfg(target_pointer_width = "32")]
type TaskHandle = u32;

const DAQMX_SUCCESS: i32 = 0;
const DAQMX_VAL_CFG_DEFAULT: i32 = -1;
const DAQMX_VAL_DIFF: i32 = 10106;
const DAQMX_VAL_CONT_SAMPS: i32 = 10123;
const DAQMX_VAL_RISING: i32 = 10280;
const DAQMX_VAL_GROUP_BY_CHANNEL: i32 = 0;
const DAQMX_VAL_VOLTS: i32 = 10348;

type FnCreateTask = unsafe extern "C" fn(name: *const i8, task: *mut TaskHandle) -> i32;
type FnClearTask = unsafe extern "C" fn(task: TaskHandle) -> i32;
type FnStartTask = unsafe extern "C" fn(task: TaskHandle) -> i32;
type FnStopTask = unsafe extern "C" fn(task: TaskHandle) -> i32;
type FnCreateAIVoltageChan = unsafe extern "C" fn(
    task: TaskHandle,
    phys_channels: *const i8,
    name_to_assign: *const i8,
    terminal_config: i32,
    min_val: c_double,
    max_val: c_double,
    units: i32,
    custom_scale_name: *const i8,
) -> i32;
type FnCfgSampClkTiming = unsafe extern "C" fn(
    task: TaskHandle,
    source: *const i8,
    rate: c_double,
    active_edge: i32,
    sample_mode: i32,
    samps_per_chan: u64,
) -> i32;
type FnCfgInputBuffer = unsafe extern "C" fn(task: TaskHandle, samps_per_chan: u32) -> i32;
type FnReadAnalogF64 = unsafe extern "C" fn(
    task: TaskHandle,
    samps_per_chan: i32,
    timeout: c_double,
    fill_mode: i32,
    read_array: *mut c_double,
    array_size_in_samps: u32,
    samps_per_chan_read: *mut i32,
    reserved: *mut c_void,
) -> i32;
type FnGetSysDevNames = unsafe extern "C" fn(buffer: *mut i8, buffer_size: u32) -> i32;
type FnGetExtendedErrorInfo = unsafe extern "C" fn(buffer: *mut i8, buffer_size: u32) -> i32;

struct Nidaq {
    _lib: Library,
    CreateTask: FnCreateTask,
    ClearTask: FnClearTask,
    StartTask: FnStartTask,
    StopTask: FnStopTask,
    CreateAIVoltageChan: FnCreateAIVoltageChan,
    CfgSampClkTiming: FnCfgSampClkTiming,
    CfgInputBuffer: FnCfgInputBuffer,
    ReadAnalogF64: FnReadAnalogF64,
    GetSysDevNames: FnGetSysDevNames,
    GetExtendedErrorInfo: FnGetExtendedErrorInfo,
}

impl Nidaq {
    unsafe fn load() -> Result<Self> {
        let lib = Library::new("nicaiu.dll")
            .context("Failed to load nicaiu.dll (install NI-DAQmx Runtime)")?;
        macro_rules! sym {
            ($t:ty, $name:expr) => {{
                let s: Symbol<$t> = lib.get($name)?;
                *s
            }};
        }
        Ok(Self {
            CreateTask:            sym!(FnCreateTask,            b"DAQmxCreateTask\0"),
            ClearTask:             sym!(FnClearTask,             b"DAQmxClearTask\0"),
            StartTask:             sym!(FnStartTask,             b"DAQmxStartTask\0"),
            StopTask:              sym!(FnStopTask,              b"DAQmxStopTask\0"),
            CreateAIVoltageChan:   sym!(FnCreateAIVoltageChan,   b"DAQmxCreateAIVoltageChan\0"),
            CfgSampClkTiming:      sym!(FnCfgSampClkTiming,      b"DAQmxCfgSampClkTiming\0"),
            CfgInputBuffer:        sym!(FnCfgInputBuffer,        b"DAQmxCfgInputBuffer\0"),
            ReadAnalogF64:         sym!(FnReadAnalogF64,         b"DAQmxReadAnalogF64\0"),
            GetSysDevNames:        sym!(FnGetSysDevNames,        b"DAQmxGetSysDevNames\0"),
            GetExtendedErrorInfo:  sym!(FnGetExtendedErrorInfo,  b"DAQmxGetExtendedErrorInfo\0"),
            _lib: lib,
        })
    }
    unsafe fn check(&self, code: i32) -> Result<()> {
        if code >= DAQMX_SUCCESS { return Ok(()); }
        let mut buf = vec![0i8; 4096];
        (self.GetExtendedErrorInfo)(buf.as_mut_ptr(), buf.len() as u32);
        let msg = CStr::from_ptr(buf.as_ptr()).to_string_lossy().to_string();
        bail!("NI-DAQmx error {}: {}", code, msg.trim());
    }
}

unsafe fn get_sys_dev_names(ni: &Nidaq) -> Result<String> {
    let mut buf = vec![0i8; 8192];
    ni.check((ni.GetSysDevNames)(buf.as_mut_ptr(), buf.len() as u32))?;
    Ok(CStr::from_ptr(buf.as_ptr()).to_string_lossy().trim().to_string())
}

fn build_phys_list(module: &str, ai_indices: &[u32]) -> String {
    ai_indices
        .iter()
        .map(|i| format!("{}/ai{}", module, i))
        .collect::<Vec<_>>()
        .join(",")
}

unsafe fn probe_channels(ni: &Nidaq, phys: &str) -> Result<()> {
    let mut t: TaskHandle = 0;
    ni.check((ni.CreateTask)(std::ptr::null(), &mut t))?;
    let phys_c = CString::new(phys).unwrap();
    let res = (ni.CreateAIVoltageChan)(
        t,
        phys_c.as_ptr(),
        std::ptr::null(),
        DAQMX_VAL_CFG_DEFAULT,
        -10.0,
        10.0,
        DAQMX_VAL_VOLTS,
        std::ptr::null(),
    );
    let _ = (ni.ClearTask)(t);
    if res < 0 {
        bail!("probe failed");
    }
    Ok(())
}

unsafe fn discover_cdaq_channels(ni: &Nidaq, prefer_mod: u32, ai_indices: &[u32]) -> Result<String> {
    let names = get_sys_dev_names(ni)?;
    let cdaq_names: Vec<&str> = names
        .split(',')
        .map(|s| s.trim())
        .filter(|s| s.starts_with("cDAQ"))
        .collect();
    if cdaq_names.is_empty() {
        bail!("No cDAQ devices reported by DAQmx (saw: {})", names);
    }
    let mut try_mods = vec![prefer_mod, 1, 2, 3, 4];
    try_mods.dedup();
    for base in cdaq_names {
        for m in &try_mods {
            let phys = build_phys_list(&format!("{}Mod{}", base, m), ai_indices);
            if probe_channels(ni, &phys).is_ok() {
                return Ok(phys);
            }
        }
    }
    bail!("Could not add requested channels on any cDAQ-9191 Mod1..4");
}

//
// ====================== DATA SOURCES ========================
//
#[derive(Clone)]
struct CdaqSample {
    t: Instant,
    values: Vec<f64>, // ai0/2/4/6 order
}

#[derive(Clone)]
struct PendoSample {
    t: Instant,
    values: [f64; 4], // P1..P4
}

trait SourceCdaq: Send + 'static {
    fn run(self: Box<Self>, tx: mpsc::Sender<CdaqSample>, run: Arc<AtomicBool>) -> Result<()>;
}
trait SourcePendo: Send + 'static {
    fn run(self: Box<Self>, tx: mpsc::Sender<PendoSample>, run: Arc<AtomicBool>) -> Result<()>;
}

struct NidaqSource;
impl SourceCdaq for NidaqSource {
    fn run(self: Box<Self>, tx: mpsc::Sender<CdaqSample>, run: Arc<AtomicBool>) -> Result<()> {
        unsafe {
            let ni = Nidaq::load()?;
            let chans = discover_cdaq_channels(&ni, CDAQ_PREFERRED_MOD, CDAQ_AI_INDICES)?;
            println!(
                "[{}] Using channels: {}",
                Local::now().format("%H:%M:%S"),
                chans
            );

            let mut task: TaskHandle = 0;
            ni.check((ni.CreateTask)(std::ptr::null(), &mut task))?;

            let chans_c = CString::new(chans).unwrap();
            ni.check((ni.CreateAIVoltageChan)(
                task,
                chans_c.as_ptr(),
                std::ptr::null(),
                DAQMX_VAL_DIFF,
                -10.0,
                10.0,
                DAQMX_VAL_VOLTS,
                std::ptr::null(),
            ))?;

            ni.check((ni.CfgInputBuffer)(
                task,
                (CDAQ_S_RATE * CDAQ_BUFFER_SEC) as u32,
            ))?;
            ni.check((ni.CfgSampClkTiming)(
                task,
                std::ptr::null(),
                CDAQ_S_RATE,
                DAQMX_VAL_RISING,
                DAQMX_VAL_CONT_SAMPS,
                CDAQ_S_PER_READ as u64,
            ))?;

            let mut running_prev = false;
            let n_ch = CDAQ_AI_INDICES.len();
            let mut read_buf = vec![0f64; n_ch * (CDAQ_S_PER_READ as usize)];
            let mut samps_read: i32 = 0;

            println!("[{}] cDAQ worker alive", Local::now().format("%H:%M:%S"));
            loop {
                let running = run.load(Ordering::Relaxed);

                if running && !running_prev {
                    if let Err(e) = ni.check((ni.StartTask)(task)) {
                        eprintln!("cDAQ StartTask error: {e:#}");
                        thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                    running_prev = true;
                } else if !running && running_prev {
                    if let Err(e) = ni.check((ni.StopTask)(task)) {
                        eprintln!("cDAQ StopTask error: {e:#}");
                    }
                    running_prev = false;
                }

                if !running {
                    thread::sleep(Duration::from_millis(60));
                    continue;
                }

                let code = (ni.ReadAnalogF64)(
                    task,
                    CDAQ_S_PER_READ as i32,
                    CDAQ_READ_TIMEOUT,
                    DAQMX_VAL_GROUP_BY_CHANNEL,
                    read_buf.as_mut_ptr(),
                    read_buf.len() as u32,
                    &mut samps_read,
                    std::ptr::null_mut(),
                );
                if code < 0 {
                    let mut buf = vec![0i8; 1024];
                    (ni.GetExtendedErrorInfo)(buf.as_mut_ptr(), buf.len() as u32);
                    let msg = CStr::from_ptr(buf.as_ptr()).to_string_lossy().to_string();
                    eprintln!("cDAQ ReadError: {}", msg.trim());
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                let per_ch = samps_read as usize;
                if per_ch == 0 {
                    continue;
                }

                let mut means = vec![0.0f64; n_ch];
                for c in 0..n_ch {
                    let start = c * per_ch;
                    let slice = &read_buf[start..start + per_ch];
                    means[c] = slice.iter().copied().sum::<f64>() / (per_ch as f64);
                }
                let _ = tx.send(CdaqSample {
                    t: Instant::now(),
                    values: means,
                });
            }
        }
    }
}
struct CdaqSim;
impl SourceCdaq for CdaqSim {
    fn run(self: Box<Self>, tx: mpsc::Sender<CdaqSample>, run: Arc<AtomicBool>) -> Result<()> {
        println!("nicaiu.dll not found — cDAQ SIMULATED data.");
        let start = Instant::now();
        let mut last_send = Instant::now();
        loop {
            if !run.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(60));
                continue;
            }
            if last_send.elapsed() >= Duration::from_millis(250) {
                // ~4 Hz simulated cDAQ
                let t = start.elapsed().as_secs_f64();
                let vals = vec![
                    0.5 * (t * 1.5).sin(),
                    80.0 + 10.0 * (t * 0.08).sin(),
                    90.0 + 10.0 * (t * 0.06).cos(),
                    100.0 + 10.0 * (t * 0.12).sin(),
                ];
                let _ = tx.send(CdaqSample {
                    t: Instant::now(),
                    values: vals,
                });
                last_send = Instant::now();
            } else {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

struct PendoReal;
impl SourcePendo for PendoReal {
    fn run(self: Box<Self>, tx: mpsc::Sender<PendoSample>, run: Arc<AtomicBool>) -> Result<()> {
        let port_path = auto_find_pendo_port().unwrap_or_else(|| "COM3".to_string());
        eprintln!("[Pendo] Using port: {}", &port_path);

        let builder = serialport::new(&port_path, PT_BAUD)
            .timeout(PT_READ_TIMEOUT)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .flow_control(serialport::FlowControl::None);

        let mut port = builder
            .open()
            .map_err(|e| anyhow::anyhow!("open {}: {}", port_path, e))?;

        let _ = port.write_data_terminal_ready(true);
        let _ = port.write_request_to_send(true);

        let mut rxbuf: Vec<u8> = Vec::with_capacity(8192);

        loop {
            if !run.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(60));
                continue;
            }

            let t_req = Instant::now();
            port.write_all(PT_REQUEST)?;
            port.flush()?;

            let mut attempts_left = 2;
            let mut deadline = t_req + PT_FRAME_DEADLINE;
            let mut payload: Option<Vec<u8>> = None;

            loop {
                if let Some(pl) = pendo_find_frame(&mut rxbuf) {
                    payload = Some(pl);
                    break;
                }
                let mut tmp = [0u8; 2048];
                match port.read(&mut tmp) {
                    Ok(n) if n > 0 => rxbuf.extend_from_slice(&tmp[..n]),
                    Ok(_) => {}
                    Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(e) => {
                        eprintln!("[Pendo] READ error: {}", e);
                        break;
                    }
                }
                if Instant::now() > deadline {
                    if attempts_left > 0 {
                        attempts_left -= 1;
                        thread::sleep(Duration::from_millis(80));
                        port.write_all(PT_REQUEST)?;
                        port.flush()?;
                        deadline = Instant::now() + PT_FRAME_DEADLINE;
                    } else {
                        break;
                    }
                }
            }

            if let Some(pl) = payload {
                let txt = pendo_printable(&pl);
                let chunks = pendo_split_az_chunks(&txt);
                let (p1, p2, p3, p4) = pendo_extract_values(&chunks);
                let _ = tx.send(PendoSample {
                    t: Instant::now(),
                    values: [p1, p2, p3, p4],
                });

                let elapsed = Instant::now().saturating_duration_since(t_req);
                if elapsed < PT_GUARD_AFTER_GOOD_FRAME {
                    thread::sleep(PT_GUARD_AFTER_GOOD_FRAME - elapsed);
                }
            } else {
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}
struct PendoSim;
impl SourcePendo for PendoSim {
    fn run(self: Box<Self>, tx: mpsc::Sender<PendoSample>, run: Arc<AtomicBool>) -> Result<()> {
        eprintln!("[Pendo] SIMULATED (no real device found).");
        let start = Instant::now();
        loop {
            if !run.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(60));
                continue;
            }
            let t = start.elapsed().as_secs_f64();
            let p1 = -1.0 + 0.1 * (t * 0.7).sin();
            let p2 = 0.3 + 0.05 * (t * 1.1).cos();
            let p3 = 20.0 + 5.0 * (t * 0.5).sin();
            let p4 = 25.0 + 7.0 * (t * 0.9).cos();
            let _ = tx.send(PendoSample {
                t: Instant::now(),
                values: [p1, p2, p3, p4],
            });
            thread::sleep(Duration::from_millis(250)); // ~4 Hz simulated Poll; UI/CSV gating will handle 2 Hz logging
        }
    }
}

// ---------- Pendo helpers ----------
fn auto_find_pendo_port() -> Option<String> {
    let ports = serialport::available_ports().ok()?;
    let mut candidates: Vec<SerialPortInfo> = ports
        .into_iter()
        .filter(|p| match &p.port_type {
            SerialPortType::UsbPort(_) => true,
            _ => true,
        })
        .collect();

    candidates.sort_by_key(|p| match &p.port_type {
        SerialPortType::UsbPort(u) => {
            let s = format!(
                "{} {}",
                u.manufacturer.as_deref().unwrap_or(""),
                u.product.as_deref().unwrap_or("")
            )
            .to_lowercase();
            if s.contains("pendotech") || s.contains("pressuremat") || s.contains("pmat") {
                0
            } else {
                1
            }
        }
        _ => 2,
    });

    for info in candidates {
        let name = info.port_name;
        if probe_is_pendotech(&name).unwrap_or(false) {
            return Some(name);
        }
    }
    None
}

fn probe_is_pendotech(port_path: &str) -> Result<bool> {
    let builder = serialport::new(port_path, PT_BAUD)
        .timeout(PT_READ_TIMEOUT)
        .data_bits(serialport::DataBits::Eight)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .flow_control(serialport::FlowControl::None);

    let mut port = match builder.open() {
        Ok(p) => p,
        Err(_) => return Ok(false),
    };

    let _ = port.write_data_terminal_ready(true);
    let _ = port.write_request_to_send(true);

    let t_req = Instant::now();
    let mut rxbuf: Vec<u8> = Vec::with_capacity(4096);

    port.write_all(PT_REQUEST)?;
    port.flush()?;

    let mut attempts_left = 1;
    let mut deadline = t_req + PT_FRAME_DEADLINE;

    loop {
        let mut tmp = [0u8; 1024];
        match port.read(&mut tmp) {
            Ok(n) if n > 0 => rxbuf.extend_from_slice(&tmp[..n]),
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return Ok(false),
        }
        if let Some(payload) = pendo_find_frame(&mut rxbuf) {
            return Ok(pendo_printable(&payload).contains("AZ,"));
        }
        if Instant::now() > deadline {
            if attempts_left > 0 {
                attempts_left -= 1;
                port.write_all(PT_REQUEST)?;
                port.flush()?;
                deadline = Instant::now() + PT_FRAME_DEADLINE;
            } else {
                return Ok(false);
            }
        }
    }
}

fn pendo_find_frame(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let mut i = None;
    for (idx, w) in buf.windows(2).enumerate() {
        if w == [DLE, STX] {
            i = Some(idx + 2);
            break;
        }
    }
    let start = i?;
    let mut j = None;
    for (idx, w) in buf[start..].windows(2).enumerate() {
        if w == [DLE, ETX] {
            j = Some(start + idx);
            break;
        }
    }
    let end = j?;
    let payload = buf[start..end].to_vec();
    buf.drain(..end + 2);
    Some(payload)
}

fn pendo_printable(bs: &[u8]) -> String {
    bs.iter()
        .map(|&x| if (32..=126).contains(&x) { x as char } else { ' ' })
        .collect()
}

fn pendo_split_az_chunks(payload_text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut i = 0;
    while let Some(pos) = payload_text[i..].find("AZ,") {
        let start = i + pos;
        let next = payload_text[start + 3..]
            .find("AZ,")
            .map(|p| start + 3 + p)
            .unwrap_or(payload_text.len());
        let ch = payload_text[start..next]
            .trim()
            .replace(char::is_whitespace, " ");
        chunks.push(ch);
        i = next;
        if i >= payload_text.len() {
            break;
        }
    }
    if chunks.is_empty() {
        let s = payload_text.trim().replace(char::is_whitespace, " ");
        if !s.is_empty() {
            chunks.push(s);
        }
    }
    chunks
}

fn pendo_extract_values(chunks: &[String]) -> (f64, f64, f64, f64) {
    let mut vals: Vec<f64> = Vec::with_capacity(4);
    for ch in chunks {
        let rev: String = ch.chars().rev().collect();
        let mut substring = if rev.len() > 19 { &rev[19..] } else { "" };
        if let Some(space_idx) = substring.find(' ') {
            substring = &substring[..space_idx];
        }
        let val_str: String = substring.chars().rev().collect();
        let v = val_str.trim().replace('−', "-").parse::<f64>().unwrap_or(0.0);
        vals.push(v);
        if vals.len() == 4 {
            break;
        }
    }
    while vals.len() < 4 {
        vals.push(0.0);
    }
    (vals[0], vals[1], vals[2], vals[3])
}

//
// ========================== UI + CSV ========================
//
struct CsvLogger {
    writer: csv::Writer<File>,
    last_flush: Instant,
}
impl CsvLogger {
    fn new() -> Result<Self> {
        // Ensure folder exists
        fs::create_dir_all(CSV_BASE).with_context(|| format!("create_dir_all {}", CSV_BASE))?;

        // Filename: A to D Data - YYYY-MM-DDTHH-MM-SS.csv
        let ts = Local::now().format("%Y-%m-%dT%H-%M-%S").to_string();
        let filename = format!("A to D Data - {ts}.csv");
        let mut path = PathBuf::from(CSV_BASE);
        path.push(filename);

        let f = File::create(&path).with_context(|| format!("create {:?}", path))?;
        let mut w = csv::Writer::from_writer(f);
        w.write_record(&["time", "ai0", "ai2", "ai4", "ai6", "P1", "P2", "P3", "P4", "Mark"])?;
        Ok(Self {
            writer: w,
            last_flush: Instant::now(),
        })
    }
    fn write_row(&mut self, _t: Instant, ai: &[f64], p: &[f64; 4]) {
        // wall-clock timestamp
        let ts_str = Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();

        let _ = self.writer.write_record(&[
            ts_str,
            format!("{:+.6}", ai.get(0).copied().unwrap_or(0.0)),
            format!("{:+.6}", ai.get(1).copied().unwrap_or(0.0)),
            format!("{:+.6}", ai.get(2).copied().unwrap_or(0.0)),
            format!("{:+.6}", ai.get(3).copied().unwrap_or(0.0)),
            format!("{:+.6}", p[0]),
            format!("{:+.6}", p[1]),
            format!("{:+.6}", p[2]),
            format!("{:+.6}", p[3]),
            "".to_string(), // Mark empty for normal rows
        ]);
        if self.last_flush.elapsed() > Duration::from_secs(1) {
            let _ = self.writer.flush();
            self.last_flush = Instant::now();
        }
    }
    fn write_mark_only(&mut self) {
        let ts_str = Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        let _ = self.writer.write_record(&[
            ts_str,
            "".to_string(),
            "".to_string(),
            "".to_string(),
            "".to_string(),
            "".to_string(),
            "".to_string(),
            "".to_string(),
            "".to_string(),
            "1".to_string(), // Mark flag
        ]);
        let _ = self.writer.flush();
    }
}

struct ScopeApp {
    // channels
    tx_cdaq: mpsc::Sender<CdaqSample>,
    rx_cdaq: mpsc::Receiver<CdaqSample>,
    tx_pendo: mpsc::Sender<PendoSample>,
    rx_pendo: mpsc::Receiver<PendoSample>,

    // sources (spawn once)
    cdaq_src: Option<Box<dyn SourceCdaq>>,
    pendo_src: Option<Box<dyn SourcePendo>>,

    // run flag (start/stop)
    run_flag: Arc<AtomicBool>,
    spawned: bool,

    // histories
    cdaq_full: Vec<VecDeque<(f64, f64)>>,  // per channel
    pendo_full: [VecDeque<(f64, f64)>; 4], // P1..P4

    // last-known values for side panel (raw)
    last_cdaq: Vec<f64>,
    last_pendo: [f64; 4],

    // display gains for cDAQ (applied to plots only)
    cdaq_gains: [f64; 4], // maps ai0, ai2, ai4, ai6

    start: Instant,

    // windowing
    detach_plots: bool,

    // CSV logger
    csv: CsvLogger,

    // marks (relative seconds since start)
    marks: Vec<f64>,

    // gating to get EXACTLY ~2 Hz CSV from ~4 Hz cDAQ input:
    cdaq_tick_index: u64,
}

impl ScopeApp {
    fn new(
        tx_cdaq: mpsc::Sender<CdaqSample>,
        rx_cdaq: mpsc::Receiver<CdaqSample>,
        tx_pendo: mpsc::Sender<PendoSample>,
        rx_pendo: mpsc::Receiver<PendoSample>,
        cdaq_src: Box<dyn SourceCdaq>,
        pendo_src: Box<dyn SourcePendo>,
        run_flag: Arc<AtomicBool>,
    ) -> Result<Self> {
        let mut cdaq_full = Vec::new();
        cdaq_full.resize_with(CDAQ_AI_INDICES.len(), || VecDeque::with_capacity(1 << 15));
        let pendo_full: [VecDeque<(f64, f64)>; 4] = [
            VecDeque::with_capacity(1 << 14),
            VecDeque::with_capacity(1 << 14),
            VecDeque::with_capacity(1 << 14),
            VecDeque::with_capacity(1 << 14),
        ];
        Ok(Self {
            tx_cdaq,
            rx_cdaq,
            tx_pendo,
            rx_pendo,
            cdaq_src: Some(cdaq_src),
            pendo_src: Some(pendo_src),
            run_flag,
            spawned: false,
            cdaq_full,
            pendo_full,
            last_cdaq: vec![0.0; CDAQ_AI_INDICES.len()],
            last_pendo: [0.0; 4],
            cdaq_gains: [1.0, 1.0, 1.0, 1.0],
            start: Instant::now(),
            detach_plots: false,
            csv: CsvLogger::new()?,
            marks: Vec::new(),
            cdaq_tick_index: 0,
        })
    }

    fn prune_full(&mut self, now_t: f64) {
        for hist in &mut self.cdaq_full {
            while let Some(&(t0, _)) = hist.front() {
                if now_t - t0 > FULL_WINDOW_SECS {
                    hist.pop_front();
                } else {
                    break;
                }
            }
        }
        for hist in &mut self.pendo_full {
            while let Some(&(t0, _)) = hist.front() {
                if now_t - t0 > FULL_WINDOW_SECS {
                    hist.pop_front();
                } else {
                    break;
                }
            }
        }
        // prune very old marks
        while let Some(&t0) = self.marks.first() {
            if now_t - t0 > FULL_WINDOW_SECS {
                self.marks.remove(0);
            } else {
                break;
            }
        }
    }

    fn clear_histories(&mut self) {
        for h in &mut self.cdaq_full {
            h.clear();
        }
        for h in &mut self.pendo_full {
            h.clear();
        }
        self.marks.clear();
        self.cdaq_tick_index = 0;
    }
}

impl eframe::App for ScopeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now_t = self.start.elapsed().as_secs_f64();

        // Controls (top bar)
        egui::TopBottomPanel::top("top_controls").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("▶ Start / Resume").clicked() {
                    if !self.spawned {
                        self.spawned = true;

                        if let Some(src) = self.cdaq_src.take() {
                            let tx = self.tx_cdaq.clone();
                            let run = self.run_flag.clone();
                            thread::spawn(move || {
                                if let Err(e) = src.run(tx, run) {
                                    eprintln!("cDAQ worker error: {e:#}");
                                }
                            });
                        }
                        if let Some(src) = self.pendo_src.take() {
                            let tx = self.tx_pendo.clone();
                            let run = self.run_flag.clone();
                            thread::spawn(move || {
                                if let Err(e) = src.run(tx, run) {
                                    eprintln!("Pendo worker error: {e:#}");
                                }
                            });
                        }
                    }
                    self.run_flag.store(true, Ordering::Relaxed);
                }

                if ui.button("⏸ Stop").clicked() {
                    self.run_flag.store(false, Ordering::Relaxed);
                }

                if ui.add_enabled(true, egui::Button::new("■ Clear history")).clicked() {
                    self.clear_histories();
                }

                // Mark button: add mark + write mark row to CSV
                if ui.button("⭐ Mark").clicked() {
                    let t = self.start.elapsed().as_secs_f64();
                    self.marks.push(t);
                    self.csv.write_mark_only();
                }

                ui.separator();
                ui.checkbox(&mut self.detach_plots, "Detach plots into floating windows");

                ui.separator();
                ui.label(format!("Now: {}", Local::now().format("%H:%M:%S")));
            });
        });

        // Drain samples (CSV only from the cDAQ loop, and only every other cDAQ tick → ~2 Hz)
        while let Ok(s) = self.rx_cdaq.try_recv() {
            let t = Instant::now();
            let rel = self.start.elapsed().as_secs_f64();
            for (ci, v) in s.values.iter().enumerate() {
                self.cdaq_full[ci].push_back((rel, *v));
            }
            self.last_cdaq = s.values.clone();

            // Gate CSV to 2 Hz: write only on even tick indices (0,2,4,…)
            if self.cdaq_tick_index % 2 == 0 {
                self.csv.write_row(t, &self.last_cdaq, &self.last_pendo);
            }
            self.cdaq_tick_index = self.cdaq_tick_index.wrapping_add(1);
        }
        while let Ok(s) = self.rx_pendo.try_recv() {
            let rel = self.start.elapsed().as_secs_f64();
            for (i, v) in s.values.iter().enumerate() {
                self.pendo_full[i].push_back((rel, *v));
            }
            // Do NOT write CSV here — cDAQ loop controls CSV cadence (2 Hz)
            self.last_pendo = s.values;
        }
        self.prune_full(now_t);

        // Side readouts + display gain sliders (0.01 ..= 1000.0)
        egui::SidePanel::right("side_readouts")
            .resizable(true)
            .default_width(260.0)
            .show(ctx, |ui| {
                ui.heading("Live Values (raw)");
                ui.separator();
                ui.label("cDAQ (V):");
                for (idx, ch) in CDAQ_AI_INDICES.iter().enumerate() {
                    ui.label(format!("ai{}: {:+.4}", ch, self.last_cdaq[idx]));
                }
                ui.separator();
                ui.label("Display Gain (plots only)");
                ui.add(egui::Slider::new(&mut self.cdaq_gains[0], 0.01..=1000.0).text("ai0 ×"));
                ui.add(egui::Slider::new(&mut self.cdaq_gains[1], 0.01..=1000.0).text("ai2 ×"));
                ui.add(egui::Slider::new(&mut self.cdaq_gains[2], 0.01..=1000.0).text("ai4 ×"));
                ui.add(egui::Slider::new(&mut self.cdaq_gains[3], 0.01..=1000.0).text("ai6 ×"));
                ui.small("These multipliers affect charts only; data stays raw.");
                ui.separator();
                ui.label("PendoTech (psi):");
                let labels = ["P1", "P2", "P3", "P4"];
                for i in 0..4 {
                    ui.label(format!("{}: {:+.3}", labels[i], self.last_pendo[i]));
                }
                ui.separator();
                ui.label(format!("Marks: {}", self.marks.len()));
            });

        // Plots (central splitter or floating windows)
        if self.detach_plots {
            egui::Window::new("cDAQ - last 30 s").show(ctx, |ui| {
                plot_cdaq_live(ui, now_t, &self.cdaq_full, &self.cdaq_gains, &self.marks);
            });
            egui::Window::new("cDAQ - full history").show(ctx, |ui| {
                plot_cdaq_full(ui, now_t, &self.cdaq_full, &self.cdaq_gains, &self.marks);
            });
            egui::Window::new("PendoTech - full history").show(ctx, |ui| {
                plot_pendo_full(ui, now_t, &self.pendo_full, &self.marks);
            });
            egui::CentralPanel::default().show(ctx, |_ui| {});
        } else {
            egui::CentralPanel::default().show(ctx, |ui| {
                StripBuilder::new(ui)
                    .size(egui_extras::Size::relative(0.33).at_least(140.0))
                    .size(egui_extras::Size::relative(0.33).at_least(140.0))
                    .size(egui_extras::Size::remainder().at_least(140.0))
                    .vertical(|mut strip| {
                        strip.cell(|ui| {
                            ui.heading("cDAQ (last 30 s)");
                            plot_cdaq_live(ui, now_t, &self.cdaq_full, &self.cdaq_gains, &self.marks);
                        });
                        strip.cell(|ui| {
                            ui.heading("cDAQ (full history, up to 12 h)");
                            plot_cdaq_full(ui, now_t, &self.cdaq_full, &self.cdaq_gains, &self.marks);
                        });
                        strip.cell(|ui| {
                            ui.heading("PendoTech Pressures (full history, up to 12 h)");
                            plot_pendo_full(ui, now_t, &self.pendo_full, &self.marks);
                        });
                    });
            });
        }

        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

// ---- Plot helpers (fixed axes + display gains + marks) ----
fn draw_marks(plot_ui: &mut egui_plot::PlotUi, marks: &[f64]) {
    for &x in marks {
        plot_ui.vline(
            VLine::new(x)
                .stroke(egui::Stroke::new(2.0, egui::Color32::YELLOW))
        );
    }
}

// cDAQ colors per channel (ai0=orange, ai2=blue, ai4=green, ai6=red)
fn cdaq_color_for_index(i: usize) -> egui::Color32 {
    match i {
        0 => egui::Color32::from_rgb(255, 165, 0), // orange
        1 => egui::Color32::from_rgb(0, 120, 215), // blue (Windows-ish)
        2 => egui::Color32::from_rgb(0, 180, 0),   // green
        3 => egui::Color32::from_rgb(220, 0, 0),   // red
        _ => egui::Color32::WHITE,
    }
}

// Pendo colors (P1=red, P2=orange, P3=green, P4=blue)
fn pendo_color_for_index(i: usize) -> egui::Color32 {
    match i {
        0 => egui::Color32::from_rgb(220, 0, 0),   // red
        1 => egui::Color32::from_rgb(255, 165, 0), // orange
        2 => egui::Color32::from_rgb(0, 180, 0),   // green
        3 => egui::Color32::from_rgb(0, 120, 215), // blue
        _ => egui::Color32::WHITE,
    }
}

fn plot_cdaq_live(
    ui: &mut egui::Ui,
    now_t: f64,
    cdaq_full: &Vec<VecDeque<(f64, f64)>>,
    gains: &[f64; 4],
    marks: &[f64],
) {
    let mut lines: Vec<Line> = Vec::new();
    for (ci, hist) in cdaq_full.iter().enumerate() {
        let g = gains[ci];
        let pts: PlotPoints = hist
            .iter()
            .filter_map(|(t, v)| if now_t - *t <= LIVE_WINDOW_SECS { Some([*t, *v * g]) } else { None })
            .collect();
        lines.push(
            Line::new(pts)
                .color(cdaq_color_for_index(ci))
        );
    }
    Plot::new("plot_cdaq_live")
        .legend(Legend::default().position(egui_plot::Corner::LeftTop))
        .allow_zoom(false)
        .allow_drag(false)
        .allow_scroll(false)
        .show(ui, |plot_ui| {
            let x0 = (now_t - LIVE_WINDOW_SECS).max(0.0);
            let x1 = now_t.max(x0 + 1.0);
            plot_ui.set_plot_bounds(PlotBounds::from_min_max([x0, Y_CDAQ_MIN], [x1, Y_CDAQ_MAX]));
            for (i, ln) in lines.into_iter().enumerate() {
                plot_ui.line(ln.name(format!("ai{} (×{:.2})", CDAQ_AI_INDICES[i], gains[i])));
            }
            draw_marks(plot_ui, marks);
        });
}

fn plot_cdaq_full(
    ui: &mut egui::Ui,
    now_t: f64,
    cdaq_full: &Vec<VecDeque<(f64, f64)>>,
    gains: &[f64; 4],
    marks: &[f64],
) {
    let mut lines: Vec<Line> = Vec::new();
    for (ci, hist) in cdaq_full.iter().enumerate() {
        let g = gains[ci];
        let pts: PlotPoints = hist.iter().map(|(t, v)| [*t, *v * g]).collect();
        lines.push(
            Line::new(pts)
                .color(cdaq_color_for_index(ci))
        );
    }
    Plot::new("plot_cdaq_full")
        .legend(Legend::default().position(egui_plot::Corner::LeftTop))
        .allow_zoom(false)
        .allow_drag(false)
        .allow_scroll(false)
        .show(ui, |plot_ui| {
            let span = now_t.min(FULL_WINDOW_SECS);
            let x0 = (now_t - span).max(0.0);
            let x1 = now_t.max(x0 + 1.0);
            plot_ui.set_plot_bounds(PlotBounds::from_min_max([x0, Y_CDAQ_MIN], [x1, Y_CDAQ_MAX]));
            for (i, ln) in lines.into_iter().enumerate() {
                plot_ui.line(ln.name(format!("ai{} (×{:.2})", CDAQ_AI_INDICES[i], gains[i])));
            }
            draw_marks(plot_ui, marks);
        });
}

fn plot_pendo_full(
    ui: &mut egui::Ui,
    now_t: f64,
    pendo_full: &[VecDeque<(f64, f64)>; 4],
    marks: &[f64],
) {
    let mut lines: Vec<Line> = Vec::new();
    for (i, hist) in pendo_full.iter().enumerate() {
        let pts: PlotPoints = hist.iter().map(|(t, v)| [*t, *v]).collect();
        lines.push(
            Line::new(pts)
                .color(pendo_color_for_index(i))
        );
    }
    Plot::new("plot_pendo_full")
        .legend(Legend::default().position(egui_plot::Corner::LeftTop))
        .allow_zoom(false)
        .allow_drag(false)
        .allow_scroll(false)
        .show(ui, |plot_ui| {
            let span = now_t.min(FULL_WINDOW_SECS);
            let x0 = (now_t - span).max(0.0);
            let x1 = now_t.max(x0 + 1.0);
            plot_ui.set_plot_bounds(PlotBounds::from_min_max([x0, Y_PENDO_MIN], [x1, Y_PENDO_MAX]));
            for (i, ln) in lines.into_iter().enumerate() {
                let name = ["P1", "P2", "P3", "P4"][i];
                plot_ui.line(ln.name(name));
            }
            draw_marks(plot_ui, marks);
        });
}

//
// ========================== MAIN ============================
//
fn main() -> Result<()> {
    let (tx_cdaq, rx_cdaq) = mpsc::channel::<CdaqSample>();
    let (tx_pendo, rx_pendo) = mpsc::channel::<PendoSample>();

    // pick sources (real vs sim) at startup
    let cdaq_src: Box<dyn SourceCdaq> = unsafe {
        if Library::new("nicaiu.dll").is_ok() {
            Box::new(NidaqSource)
        } else {
            Box::new(CdaqSim)
        }
    };
    let pendo_src: Box<dyn SourcePendo> = if auto_find_pendo_port().is_some() {
        Box::new(PendoReal)
    } else {
        Box::new(PendoSim)
    };

    let run_flag = Arc::new(AtomicBool::new(false)); // start paused

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(egui::vec2(1200.0, 820.0))
            .with_min_inner_size(egui::vec2(900.0, 600.0))
            .with_title("cDAQ + PendoTech Scope"),
        ..Default::default()
    };

    eframe::run_native(
        "cDAQ + PendoTech Scope",
        native_options,
        Box::new({
            let run_flag = run_flag.clone();
            move |_cc| {
                Box::new(
                    ScopeApp::new(
                        tx_cdaq.clone(),
                        rx_cdaq,
                        tx_pendo.clone(),
                        rx_pendo,
                        cdaq_src,
                        pendo_src,
                        run_flag,
                    )
                    .expect("init app"),
                )
            }
        }),
    )
    .unwrap();

    Ok(())
}