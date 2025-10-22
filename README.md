# PendoTECH PressureMAT Serial Data Logger

Read live pressures from a PendoTECH PressureMAT over RS-232 — **no vendor software required**.  

- 📄 Outputs a timestamped CSV file  
- 🖥️ Prints live readings to the console  

**Supported Devices:**
- PMAT2
- PMAT2P
- PMAT3
- PMAT3P
- PMAT4A
- PMAT4R
- PMAT3A
- PMAT2A
- PMAT2F
- PMAT2HR
- PMAT-S
- PMAT-SHR
- PMAT-DAQ

><u>⚠️ **Disclaimer:**</u> 
> This project is **not affiliated with PendoTECH**.It communicates directly with the device’s serial **report stream** and does not use or bundle any PendoTECH software or licenses.

## Hardware and Setup

**You will need:**

- A standard USB-to-RS-232 adapter cable  
- A PressureMAT (PMAT) device  
- A computer  

> <u> ⚙️ **Note**</u>  
> This code was tested only on the **PressureMAT 4A**.  
> However, it should work with any of the above models, provided they use RS-232 and follow the same communication protocol.

## Device Setup (on the PressureMAT)

On the PressureMAT menus, for each channel you want to read:

- Navigate to:  
  **Input Programming → Comm Port = Sio Report**

- If there’s a **global reporting interval**, you may enable/start it.  
  (Polling will still work even without a periodic auto-report.)


> <u> ⚙️ **Note**</u>  
> This script actively polls the device using the `AZK` command and does **NOT** depend on the auto-report cadence. 

## Usage

1. **Close** any vendor GUI that might be holding the COM port.  
2. **Connect** the RS-232 cable from the PressureMAT to the computer's USB port.   
> ⚙️ <u> **Note (Windows):** </u>  
> You can find the correct COM port number by opening:  
> **Device Manager → Ports (COM & LPT)**  
> Look for your **USB-to-RS232 adapter** (e.g., `USB Serial Port (COM3)`),  
> and update the `PORT` value in the script accordingly.
4. **Generate Compiled Executable** the script:

```bash
cargo run --release
```
5. **Launch** the `.exe` file found in releases

```bash
cd target/release
./your_project_name.exe   # or just double-click in Explorer
```

### Pendotech Format

This is how the script receives information from the Pendotech, with the following columns showing preparsed data, and the last column showing example of the raw bytes coming from the device:

| time (local) | p1    | p2    | p3    | p4    | raw                                   |
|--------------|-------|-------|-------|-------|---------------------------------------|
| 09:14:10.472 | 0.000 | 0.200 | -0.500| 0.000 | AZ,00000.01,4, ... AZ,00000.03,4, ... |

- **time (local):** Timestamp when the frame was received (milliseconds precision)  
- **p1–p4:** Parsed pressure values from the device (units depend on PMAT config)  
- **raw:** Raw RS-232 frame(s) as received from the PressureMAT




## What it Does
<li>Opens a serial port to a PressureMAT over USB.</li>
<li>Polls using the AZK command (half-duplex request/response).</li>
<li>Parses the framed reply (DLE/STX … DLE/ETX) and extracts P1–P4 from the AZ,... rows.</li>
<l>Logs to pmat_poll.csv with columns: time, p1, p2, p3, p4, raw</l>
<li>time is local wall-clock HH:MM:SS.mmm</li>
<li>raw contains the compacted AZ,... chunks for traceability</li>
<li>Prints live values to the console.</li>

### Serial Settings

The script defaults to:

- **Port:** `COM3` (change `PORT` in the code if needed)  
- **Baud rate:** `9600`  
- **Format:** `8-N-1`

> <u> ⚙️ **Note**</u>  
> Some documentation or older tools mention `1200 7-E-1` or `7-O-1`.  
> In testing, only **9600 8-N-1** worked reliably.  
> If you get no replies, try the legacy settings by updating `BAUD`, `parity`, `bytesize`, and `stopbits` in the code, and/or use a null-modem adapter.

### Protocol Notes

The PressureMAT wraps payloads in a framed message:

```
DLE STX … DLE ETX
```

Inside the frame are one or more `AZ,...` rows. Example:

```
AZ,00000.01,4,        0.0,        0.0,        0.5,        0.5,01734,X,X,X,X,X,18
AZ,00000.03,4,        0.0,        0.0,        0.2,        0.2,01734,X,X,X,X,X,1C
AZ,00000.05,4,        0.0,        0.0,       -0.5,       -0.5,01734,X,X,X,X,X,FA
AZ,00000.07,4,        0.0,        0.0,        0.0,        0.0,01734,X,X,X,X,X,1C
```

- `.01 → P1`
- `.03 → P2`
- `.05 → P3`
- `.07 → P4`

The script extracts the **last numeric field** from each chunk (the pressure value), being tolerant of:
- Extra spaces
- Occasional Unicode minus signs

It polls with `AZK` and enforces a **small inter-request gap** (half-duplex etiquette) to avoid overruns.


## Safety & Disclaimers

Use at your own risk. Validate readings against your process requirements. This project is for data access and observability and does not control the PressureMAT.

# Pendotech-Reader-Win (cDAQ + PendoTech Scope — Code Walkthrough)

This document explains **what each constant, struct, and function does** in the provided Rust program. It’s meant to be a drop-in `README.md` so a new contributor (even if not NI-DAQmx-savvy) can orient quickly.

---

## High-Level Overview

This app:

- Reads **analog voltages** from a **NI cDAQ** (NI-DAQmx via FFI).
- Polls a **PendoTech PressureMAT** over **serial** to get P1–P4 pressures.
- Displays **live plots** and **full-history plots** (up to 12 hours) using `egui`/`egui_plot`.
- Logs to a **CSV** at a fixed cadence (~**2 Hz**) with optional **marks** (events).

If **hardware is missing**, it auto-switches to **simulated data** so the UI can be tested anywhere.

---

## Configuration Constants

- **cDAQ**
  - `CDAQ_S_RATE`: Sample rate (Hz).
  - `CDAQ_S_PER_READ`: Samples per read call (per channel). With 200 Hz and 50 samples ⇒ ~4 Hz updates.
  - `CDAQ_READ_TIMEOUT`: Read timeout in seconds.
  - `CDAQ_BUFFER_SEC`: Input buffer depth in seconds.
  - `CDAQ_PREFERRED_MOD`: Preferred cDAQ module number to probe first (e.g., `Mod1`).
  - `CDAQ_AI_INDICES`: Which AI indices to read (here, `ai0/2/4/6`).

- **PendoTech**
  - `PT_BAUD`, `PT_READ_TIMEOUT`, `PT_FRAME_DEADLINE`, `PT_GUARD_AFTER_GOOD_FRAME`:
    Serial settings and timing guards around request/response cycles.
  - `PT_REQUEST`: The request bytes to trigger a PendoTech response.
  - `DLE/STX/ETX`: Frame markers for decoding framed payloads.

- **Windows & Axes**
  - `LIVE_WINDOW_SECS`: Live view time span (30 s).
  - `FULL_WINDOW_SECS`: Long history span (12 h).
  - `Y_*`: Fixed plot y-ranges for cDAQ and PendoTech.

- **CSV destination**
  - `CSV_BASE`: Base directory for CSV files (ensure it exists / is writable).

---

## NI-DAQmx FFI Layer

These types and bindings let us call NI-DAQmx functions in `nicaiu.dll`.

- `TaskHandle`: Platform-width integer for DAQmx task handles.
- `DAQMX_*` constants: DAQmx numeric flags/enums (e.g., terminal config, units, modes).
- `Fn*` type aliases: Rust signatures for DAQmx C functions (CreateTask, ReadAnalogF64, etc.).

### `struct Nidaq`
Holds:
- The loaded `Library` (`nicaiu.dll`).
- Function pointers for each DAQmx call we use.

### `impl Nidaq`
- `unsafe fn load() -> Result<Self>`  
  Loads `nicaiu.dll`, resolves and stores all required symbols.  
  **Errors** if `nicaiu.dll` isn’t installed (suggests installing NI-DAQmx Runtime).

- `unsafe fn check(&self, code: i32) -> Result<()>`  
  Convenience: if DAQmx returns an error code, query extended error info and convert to `anyhow::Error`.

### Free FFI Helpers
- `unsafe fn get_sys_dev_names(ni: &Nidaq) -> Result<String>`  
  Calls `DAQmxGetSysDevNames` and returns a comma-separated device list (e.g., `cDAQ-9191, ...`).

- `fn build_phys_list(module: &str, ai_indices: &[u32]) -> String`  
  Builds `"cDAQxMody/aiA,cDAQxMody/aiB,..."` strings for channel creation.

- `unsafe fn probe_channels(ni: &Nidaq, phys: &str) -> Result<()>`  
  Creates a temp task with requested voltage channels to verify they are valid; errors if DAQmx rejects.

- `unsafe fn discover_cdaq_channels(ni: &Nidaq, prefer_mod: u32, ai_indices: &[u32]) -> Result<String>`  
  - Scans DAQmx system device names for `cDAQ*`.
  - Tries `Mod{prefer_mod}` first (then 1..4) with `probe_channels`.
  - Returns the **first valid physical channel list string** or an error if none work.

---

## Data Types and Traits

- `struct CdaqSample { t: Instant, values: Vec<f64> }`  
  cDAQ sample bundle (mean of a short block per channel) with timestamp.

- `struct PendoSample { t: Instant, values: [f64; 4] }`  
  PendoTech pressures P1..P4 with timestamp.

- `trait SourceCdaq` / `trait SourcePendo`  
  Abstract “worker” that **produces samples** on a channel while `run_flag` is true.

### cDAQ Concrete Sources
- `struct NidaqSource;`
  - `impl SourceCdaq for NidaqSource::run(...)`  
    Creates/starts a DAQmx task for the discovered channels.  
    - Configures differential inputs, buffer, sample clock.  
    - In a loop:
      - Starts/stops based on `run_flag`.
      - Calls `ReadAnalogF64` to fetch blocks (`CDAQ_S_PER_READ` per channel).
      - Computes **per-channel means** (reducing data rate).
      - Sends `CdaqSample` over `tx`.
      - Gently retries on read errors.

- `struct CdaqSim;`
  - `impl SourceCdaq for CdaqSim::run(...)`  
    Simulates ~4 Hz cDAQ data with a few sinusoids so the plots/CSV pipeline can be exercised without hardware.

### PendoTech Concrete Sources
- `struct PendoReal;`
  - `impl SourcePendo for PendoReal::run(...)`  
    Opens a serial port (auto-detected or defaults to `COM3`), sends `PT_REQUEST`, and decodes framed responses.  
    - Requests once per cycle, with deadline/attempt logic.  
    - On a valid frame, parses pressure values (P1..P4) and transmits `PendoSample`.  
    - Adds a small **guard delay** after a good frame to avoid hammering the device.

- `struct PendoSim;`
  - `impl SourcePendo for PendoSim::run(...)`  
    Generates synthetic P1..P4 waveforms at ~4 Hz.

---

## PendoTech Serial Helpers

- `fn auto_find_pendo_port() -> Option<String>`  
  Enumerates serial ports, heuristically orders likely PendoTech devices first, and calls `probe_is_pendotech` to confirm.

- `fn probe_is_pendotech(port_path: &str) -> Result<bool>`  
  Opens the port, sends a single request, looks for a framed payload containing `"AZ,"`.

- `fn pendo_find_frame(buf: &mut Vec<u8>) -> Option<Vec<u8>>`  
  Finds a `DLE STX` … `DLE ETX` framed payload in a rolling buffer, returns the **payload bytes** and **drains** processed bytes.

- `fn pendo_printable(bs: &[u8]) -> String`  
  Converts payload bytes to a printable ASCII string (non-printables → space).

- `fn pendo_split_az_chunks(payload_text: &str) -> Vec<String>`  
  Splits text into `AZ, ...` chunks (robust to partials), whitespace-normalized.

- `fn pendo_extract_values(chunks: &[String]) -> (f64, f64, f64, f64)`  
  Heuristic that extracts the **last 4 numeric values** from `AZ,` chunks (handles Unicode minus, missing values → 0.0).

---

## CSV Logger

- `struct CsvLogger { writer: csv::Writer<File>, last_flush: Instant }`  
  Manages a CSV file in `CSV_BASE` named:  
  **`A to D Data - YYYY-MM-DDTHH-MM-SS.csv`**

- `impl CsvLogger`
  - `fn new() -> Result<Self>`  
    Creates the folder/file, writes CSV header:  
    `time, ai0, ai2, ai4, ai6, P1, P2, P3, P4, Mark`.

  - `fn write_row(&mut self, _t: Instant, ai: &[f64], p: &[f64; 4])`  
    Appends a **data row** with wall-clock timestamp and values.  
    Flushes at most once per second for durability without over-syncing.

  - `fn write_mark_only(&mut self)`  
    Appends a **mark row** (only timestamp + `Mark=1`) and flushes immediately.  
    Used by the UI’s ⭐ **Mark** button for event annotations.

> **Note on cadence:** The app writes CSV rows **only** in the cDAQ receive loop, and **every other** cDAQ tick → ~2 Hz logging.

---

## UI/State: `ScopeApp`

Holds channels, sources, state, histories, and plotting options.

### Fields (selected)
- `tx_* / rx_*`: MPSC channels for cDAQ and PendoTech samples.
- `cdaq_src / pendo_src`: One-shot sources (real or sim), moved into worker threads on first Start.
- `run_flag`: Shared `AtomicBool` to start/stop workers.
- `cdaq_full: Vec<VecDeque<(f64,f64)>>`: Per-channel history `(t, value)` for cDAQ.
- `pendo_full: [VecDeque<(f64,f64)>; 4]`: Histories for P1..P4.
- `last_cdaq`, `last_pendo`: Last raw values (for side readouts).
- `cdaq_gains: [f64;4]`: Display multipliers (plots only, **not** CSV).
- `csv: CsvLogger`: CSV writer.
- `marks: Vec<f64>`: Event timestamps (relative seconds since start).
- `cdaq_tick_index`: Used to gate CSV to **2 Hz**.

### Methods
- `fn new(...) -> Result<Self>`  
  Initializes queues, histories, gains, CSV writer, and state.

- `fn prune_full(&mut self, now_t: f64)`  
  Drops old points beyond `FULL_WINDOW_SECS` for all histories and trims old marks.

- `fn clear_histories(&mut self)`  
  Clears histories, marks, and resets `cdaq_tick_index` (without touching CSV file).

### `impl eframe::App for ScopeApp`
- `fn update(&mut self, ctx, frame)`  
  Main UI loop:
  1. **Top bar controls**  
     - ▶ **Start/Resume**: on first click, spawns worker threads for sources; sets `run_flag = true`.  
     - ⏸ **Stop**: pauses workers (`run_flag = false`).  
     - ■ **Clear history**: clears in-memory histories (CSV untouched).  
     - ⭐ **Mark**: pushes a mark and writes a mark-only CSV row.  
     - **Detach plots** toggle: show plots in floating windows vs stacked panels.
  2. **Drains cDAQ receiver**  
     - Appends samples to histories; updates `last_cdaq`.  
     - **CSV cadence**: writes a data row **only on even** `cdaq_tick_index` (i.e., ~2 Hz).  
  3. **Drains Pendo receiver**  
     - Appends samples to histories; updates `last_pendo` (**no** CSV here).  
  4. **Prunes histories**, then renders:
     - **Right side** panel: live raw values, gain sliders, marks count.
     - **Center** (or floating) plots:
       - `cDAQ (last 30 s)`
       - `cDAQ (full history, up to 12 h)`
       - `PendoTech (full history, up to 12 h)`
  5. Schedules next repaint (~33 ms).

---

## Plot Helpers

- `fn draw_marks(plot_ui, marks)`  
  Draws **yellow vertical lines** at each mark time.

- **Color helpers**
  - `fn cdaq_color_for_index(i) -> Color32`  
    Fixed per-channel colors (ai0=orange, ai2=blue, ai4=green, ai6=red).
  - `fn pendo_color_for_index(i) -> Color32`  
    Fixed P1..P4 colors (P1=red, P2=orange, P3=green, P4=blue).

- `fn plot_cdaq_live(ui, now_t, cdaq_full, gains, marks)`  
  - Shows last **30 s** window, fixed y-range (`Y_CDAQ_MIN..MAX`), no zoom/drag.  
  - Applies **display gains** per channel (legend shows `×gain`).

- `fn plot_cdaq_full(ui, now_t, cdaq_full, gains, marks)`  
  - Shows **full retained history** (up to 12 h), fixed y-range, no zoom/drag.  
  - Applies display gains.

- `fn plot_pendo_full(ui, now_t, pendo_full, marks)`  
  - Full retained P1..P4 history, fixed y-range for pressures, no zoom/drag.

> Fixed axes prevent accidental re-scaling and make screens comparable between runs.

---

## Program Entry Point

### `fn main() -> Result<()>`
1. Creates cDAQ/PendoTech channels:  
   `let (tx_cdaq, rx_cdaq) = mpsc::channel::<CdaqSample>();`  
   `let (tx_pendo, rx_pendo) = mpsc::channel::<PendoSample>();`

2. **Source selection (real vs sim)**:
   - cDAQ: if `nicaiu.dll` loads → `NidaqSource`, else `CdaqSim`.
   - Pendo: if `auto_find_pendo_port()` is `Some` → `PendoReal`, else `PendoSim`.

3. Builds `NativeOptions` (window title/size) and runs `eframe::run_native(...)`.
   - Inside the app factory closure, constructs `ScopeApp::new(...)`.
   - The **Start/Resume** button spawns the source worker threads once.

4. Returns `Ok(())` after the `run_native` call completes.

---

- cDAQ block averages → lower rate → UI and CSV friendly.
- PendoTech polled at ~4 Hz; **CSV cadence controlled by cDAQ** so rows stay aligned.

---

## Operational Notes

- **Permissions/Drivers**
  - cDAQ requires NI-DAQmx Runtime (`nicaiu.dll`).  
  - PendoTech requires serial port access (Windows COM ports).

- **CSV Safety**
  - Flushes at least once per second; mark rows flush immediately.
  - Change `CSV_BASE` to a writable folder for your machine.

- **Troubleshooting**
  - cDAQ: If no `cDAQ` devices found, you’ll see simulated data notice. Install NI-DAQmx or check connections.  
  - Pendo: If auto-detect fails, the app falls back to simulated pressures (check cable/driver/COM port).  
  - Performance: Fixed axes and block averaging keep UI responsive over long sessions.

---

## Quick Glossary

- **DAQmx**: National Instruments API for DAQ devices.
- **cDAQ**: CompactDAQ chassis + modules.
- **FFI**: Foreign Function Interface (Rust calling C functions).
- **egui**: Immediate-mode GUI framework.
- **egui_plot**: Plotting widget used for charts.
- **MPSC**: Multi-producer, single-consumer channel used for cross-thread messaging.

---
