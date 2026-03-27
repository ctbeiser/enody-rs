use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, ClearType},
};

use enody::{
    emitter::remote::RemoteEmitter,
    environment::Environment,
    fixture::remote::RemoteFixture,
    message::{Configuration, Flux},
    source::remote::RemoteSource,
    usb::UsbEnvironment,
};

// ── Data Structures ──

enum UIEntry {
    Source {
        label: String,
        flux: f32,
        source_index: usize,
    },
    Emitter {
        label: String,
        flux: f32,
        spectrum: Option<Vec<f32>>,
        peak_nm: f32,
        color_name: &'static str,
        handle_index: usize,
        source_entry_index: usize,
    },
}

impl UIEntry {
    fn flux(&self) -> f32 {
        match self {
            UIEntry::Source { flux, .. } | UIEntry::Emitter { flux, .. } => *flux,
        }
    }

    fn set_flux(&mut self, new: f32) {
        match self {
            UIEntry::Source { flux, .. } | UIEntry::Emitter { flux, .. } => *flux = new,
        }
    }

}

enum DeviceCommand {
    SetEmitter(usize, f32),
    SetSource(usize, f32),
}

struct EmitterHandle {
    fixture: RemoteFixture,
    source_index: usize,
    emitter: RemoteEmitter,
}

const STEP: f32 = 0.05;
const BAR_WIDTH: usize = 20;
const CHART_WIDTH: usize = 32;
const CHART_HEIGHT: usize = 6;
const MAX_VISIBLE: usize = 16;
const DEFAULT_SOURCE_FLUX: f32 = 1.0;

// ── Entry Point ──

pub async fn run() -> Result<(), enody::Error> {
    let environment = UsbEnvironment::new();
    let runtimes = environment.runtimes();

    if runtimes.is_empty() {
        println!("No Enody devices found.");
        return Ok(());
    }

    let mut ui_entries: Vec<UIEntry> = Vec::new();
    let mut handles: Vec<EmitterHandle> = Vec::new();
    let mut all_sources: Vec<RemoteSource> = Vec::new();
    let mut download_count = 0usize;
    let mut num_sources = 0usize;

    for runtime in &runtimes {
        let Ok(host) = runtime.host().await else {
            continue;
        };
        println!("Host: {} (v{})", host.identifier(), host.version());

        let Ok(fixtures) = host.fixtures().await else {
            continue;
        };

        for (fi, fixture) in fixtures.into_iter().enumerate() {
            let Ok(sources) = fixture.sources().await else {
                continue;
            };
            let base_source_index = all_sources.len();

            for (si, source) in sources.iter().enumerate() {
                let source_index = base_source_index + si;
                let source_entry_index = ui_entries.len();

                ui_entries.push(UIEntry::Source {
                    label: format!("S{}", source_index),
                    flux: DEFAULT_SOURCE_FLUX,
                    source_index,
                });
                num_sources += 1;

                let Ok(emitters) = source.emitters().await else {
                    continue;
                };

                for (ei, emitter) in emitters.into_iter().enumerate() {
                    download_count += 1;
                    print!("\rDownloading spectral data... {}", download_count);
                    let _ = io::stdout().flush();

                    let (spectrum, peak_nm) = match emitter.spectral_data().await {
                        Ok(sd) => {
                            let measurements: Vec<f32> =
                                sd.samples().iter().map(|s| s.measurement()).collect();
                            let peak = sd
                                .samples()
                                .iter()
                                .max_by(|a, b| {
                                    a.measurement()
                                        .partial_cmp(&b.measurement())
                                        .unwrap_or(std::cmp::Ordering::Equal)
                                })
                                .map(|s| s.wavelength())
                                .unwrap_or(0.0);
                            (Some(measurements), peak)
                        }
                        Err(_) => (None, 0.0),
                    };

                    let handle_index = handles.len();
                    ui_entries.push(UIEntry::Emitter {
                        label: format!("F{}S{}E{}", fi, si, ei),
                        flux: 0.0,
                        spectrum,
                        peak_nm,
                        color_name: wavelength_color(peak_nm),
                        handle_index,
                        source_entry_index,
                    });
                    handles.push(EmitterHandle {
                        fixture: fixture.clone(),
                        source_index,
                        emitter,
                    });
                }
            }

            all_sources.extend(sources);
        }
    }

    if handles.is_empty() {
        println!("\nNo emitters found.");
        return Ok(());
    }

    println!(
        "\rDownloaded spectral data for {} emitters    ",
        download_count
    );

    // ── Initialize device to known state ──

    print!("Initializing...");
    let _ = io::stdout().flush();

    for source in &all_sources {
        let _ = source
            .display(Configuration::Manual, Flux::Relative(DEFAULT_SOURCE_FLUX))
            .await;
    }
    for handle in &handles {
        let _ = handle.emitter.set_flux(Flux::Relative(0.0)).await;
    }
    if let Some(handle) = handles.first() {
        let _ = handle
            .fixture
            .display(Configuration::Manual, Flux::Relative(0.5))
            .await;
    }

    println!(" done");

    // ── Spawn device task ──

    let (tx, rx) = tokio::sync::mpsc::channel::<DeviceCommand>(64);
    let last_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let error_handle = last_error.clone();
    let device_task = tokio::spawn(async move {
        device_loop(rx, handles, all_sources, num_sources, error_handle).await;
    });

    // ── Enter TUI ──

    terminal::enable_raw_mode().map_err(to_err)?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide).map_err(to_err)?;

    let result = mixer_loop(&mut ui_entries, &tx, &last_error).await;

    let _ = execute!(stdout, cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();

    drop(tx);
    let _ = device_task.await;

    result
}

// ── Device Loop ──

async fn device_loop(
    mut rx: tokio::sync::mpsc::Receiver<DeviceCommand>,
    handles: Vec<EmitterHandle>,
    sources: Vec<RemoteSource>,
    num_sources: usize,
    last_error: Arc<Mutex<Option<String>>>,
) {
    let mut source_fluxes = vec![DEFAULT_SOURCE_FLUX; num_sources];

    while let Some(cmd) = rx.recv().await {
        let mut emitter_latest: HashMap<usize, f32> = HashMap::new();
        let mut source_latest: HashMap<usize, f32> = HashMap::new();

        match cmd {
            DeviceCommand::SetEmitter(i, f) => {
                emitter_latest.insert(i, f);
            }
            DeviceCommand::SetSource(i, f) => {
                source_latest.insert(i, f);
            }
        }
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                DeviceCommand::SetEmitter(i, f) => {
                    emitter_latest.insert(i, f);
                }
                DeviceCommand::SetSource(i, f) => {
                    source_latest.insert(i, f);
                }
            }
        }

        let mut error = None;
        let mut affected_sources: HashSet<usize> = HashSet::new();

        // Update tracked source fluxes
        for (&si, &flux) in &source_latest {
            if si < source_fluxes.len() {
                source_fluxes[si] = flux;
                affected_sources.insert(si);
            }
        }

        // Set emitter fluxes
        for (&hi, &flux) in &emitter_latest {
            if let Some(handle) = handles.get(hi) {
                if let Err(e) = handle.emitter.set_flux(Flux::Relative(flux)).await {
                    error = Some(format!("{:?}", e));
                } else {
                    affected_sources.insert(handle.source_index);
                }
            }
        }

        // Send display to all affected sources with their current master flux
        for &si in &affected_sources {
            if let Some(source) = sources.get(si) {
                let sf = source_fluxes.get(si).copied().unwrap_or(DEFAULT_SOURCE_FLUX);
                if let Err(e) = source
                    .display(Configuration::Manual, Flux::Relative(sf))
                    .await
                {
                    error = Some(format!("{:?}", e));
                }
            }
        }

        // Fixture display
        if !affected_sources.is_empty() {
            if let Some(handle) = handles.first() {
                if let Err(e) = handle
                    .fixture
                    .display(Configuration::Manual, Flux::Relative(0.5))
                    .await
                {
                    error = Some(format!("{:?}", e));
                }
            }
        }

        *last_error.lock().unwrap() = error;
    }
}

// ── Mixer Loop ──

async fn mixer_loop(
    entries: &mut [UIEntry],
    tx: &tokio::sync::mpsc::Sender<DeviceCommand>,
    last_error: &Arc<Mutex<Option<String>>>,
) -> Result<(), enody::Error> {
    let mut selected: usize = 0;
    let mut scroll_offset: usize = 0;
    let mut stdout = io::stdout();

    render(&mut stdout, entries, selected, scroll_offset, &None)?;

    loop {
        if !event::poll(Duration::from_millis(50)).map_err(to_err)? {
            let error = last_error.lock().unwrap().take();
            if error.is_some() {
                render(&mut stdout, entries, selected, scroll_offset, &error)?;
            }
            continue;
        }

        let Event::Key(key) = event::read().map_err(to_err)? else {
            continue;
        };

        if key.kind != KeyEventKind::Press {
            continue;
        }

        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            break;
        }

        let mut changed = false;

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,

            KeyCode::Up | KeyCode::Char('k') => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(entries.len() - 1);
            }

            KeyCode::Right | KeyCode::Char('l') => {
                let new = (entries[selected].flux() + STEP).min(1.0);
                entries[selected].set_flux(new);
                changed = true;
            }
            KeyCode::Left | KeyCode::Char('h') => {
                let new = (entries[selected].flux() - STEP).max(0.0);
                entries[selected].set_flux(new);
                changed = true;
            }

            KeyCode::Char('0') => {
                entries[selected].set_flux(0.0);
                changed = true;
            }
            KeyCode::Char('1') => {
                entries[selected].set_flux(1.0);
                changed = true;
            }

            _ => continue,
        }

        if changed {
            let cmd = match &entries[selected] {
                UIEntry::Source {
                    source_index, flux, ..
                } => DeviceCommand::SetSource(*source_index, *flux),
                UIEntry::Emitter {
                    handle_index, flux, ..
                } => DeviceCommand::SetEmitter(*handle_index, *flux),
            };
            let _ = tx.try_send(cmd);
        }

        let max_visible = MAX_VISIBLE.min(entries.len());
        if selected >= scroll_offset + max_visible {
            scroll_offset = selected + 1 - max_visible;
        }
        if selected < scroll_offset {
            scroll_offset = selected;
        }

        let error = last_error.lock().unwrap().take();
        render(&mut stdout, entries, selected, scroll_offset, &error)?;
    }

    Ok(())
}

// ── Rendering ──

fn render(
    stdout: &mut io::Stdout,
    entries: &[UIEntry],
    selected: usize,
    scroll_offset: usize,
    error: &Option<String>,
) -> Result<(), enody::Error> {
    execute!(
        stdout,
        cursor::MoveTo(0, 0),
        terminal::Clear(ClearType::All)
    )
    .map_err(to_err)?;

    w(stdout, " Enody Mixer\r\n")?;
    w(stdout, &format!(" {}\r\n\r\n", "\u{2500}".repeat(68)))?;

    let max_visible = MAX_VISIBLE.min(entries.len());
    let visible_end = (scroll_offset + max_visible).min(entries.len());

    if scroll_offset > 0 {
        w(stdout, &format!("   \u{2191} {} more\r\n", scroll_offset))?;
    }

    for i in scroll_offset..visible_end {
        let entry = &entries[i];
        let marker = if i == selected { ">" } else { " " };
        let flux = entry.flux();
        let filled = (flux * BAR_WIDTH as f32).round() as usize;
        let empty = BAR_WIDTH - filled;
        let bar = format!(
            "{}{}",
            "\u{2588}".repeat(filled),
            "\u{2591}".repeat(empty),
        );

        match entry {
            UIEntry::Source { label, .. } => {
                w(
                    stdout,
                    &format!(" {} {:<9} {}  {:.2}\r\n", marker, label, bar, flux),
                )?;
            }
            UIEntry::Emitter { label, .. } => {
                w(
                    stdout,
                    &format!(" {}   {:<7} {}  {:.2}\r\n", marker, label, bar, flux),
                )?;
            }
        }
    }

    if visible_end < entries.len() {
        w(
            stdout,
            &format!("   \u{2193} {} more\r\n", entries.len() - visible_end),
        )?;
    }

    w(stdout, "\r\n")?;

    // ── Spectrum charts ──

    let has_spectra = entries
        .iter()
        .any(|e| matches!(e, UIEntry::Emitter { spectrum: Some(_), .. }));

    if has_spectra {
        let sel = &entries[selected];

        let (left_title, sel_ds) = match sel {
            UIEntry::Emitter {
                label,
                peak_nm,
                color_name,
                spectrum,
                ..
            } => {
                let title = format!("{} ({:.0}nm {})", label, peak_nm, color_name);
                let ds = spectrum
                    .as_ref()
                    .map(|s| downsample(s, CHART_WIDTH))
                    .unwrap_or_else(|| vec![0.0; CHART_WIDTH]);
                (title, ds)
            }
            UIEntry::Source { label, .. } => {
                let title = format!("{} (source mix)", label);
                let spec = compute_source_spectrum(entries, selected);
                let ds = downsample(&spec, CHART_WIDTH);
                (title, ds)
            }
        };

        let total_raw = compute_total_spectrum(entries);
        let total_ds = downsample(&total_raw, CHART_WIDTH);

        let left = render_chart(&sel_ds);
        let right = render_chart(&total_ds);

        w(
            stdout,
            &format!(" {:<w$}    {}\r\n", left_title, "Mix", w = CHART_WIDTH),
        )?;

        for row in 0..CHART_HEIGHT {
            w(
                stdout,
                &format!(
                    " {:<w$}    {}\r\n",
                    left[row],
                    right[row],
                    w = CHART_WIDTH,
                ),
            )?;
        }

        let axis = format!("380nm{:>w$}", "780nm", w = CHART_WIDTH - 5);
        w(
            stdout,
            &format!(" {:<w$}    {}\r\n", axis, axis, w = CHART_WIDTH),
        )?;
        w(stdout, "\r\n")?;
    }

    if let Some(err) = error {
        w(stdout, &format!(" Error: {}\r\n\r\n", err))?;
    }

    w(
        stdout,
        " \u{2191}\u{2193}/jk select  \u{2190}\u{2192}/hl adjust  0 off  1 full  q quit\r\n",
    )?;

    stdout.flush().map_err(to_err)?;
    Ok(())
}

// ── Chart Helpers ──

fn render_chart(values: &[f32]) -> Vec<String> {
    let max_val = values.iter().cloned().fold(0.0f32, f32::max);

    (0..CHART_HEIGHT)
        .map(|row_from_top| {
            let row_from_bottom = CHART_HEIGHT - 1 - row_from_top;
            values
                .iter()
                .map(|&val| {
                    if max_val <= 0.0 {
                        return ' ';
                    }
                    let bar_h = val / max_val * CHART_HEIGHT as f32;
                    let full = bar_h as usize;
                    let frac = bar_h - full as f32;

                    if row_from_bottom < full {
                        '\u{2588}'
                    } else if row_from_bottom == full && frac > 0.1 {
                        const BLOCKS: [char; 8] = [
                            ' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}',
                            '\u{2586}', '\u{2587}',
                        ];
                        BLOCKS[((frac * 8.0) as usize).min(7)]
                    } else {
                        ' '
                    }
                })
                .collect()
        })
        .collect()
}

fn downsample(samples: &[f32], width: usize) -> Vec<f32> {
    let bucket = samples.len() as f32 / width as f32;
    (0..width)
        .map(|i| {
            let lo = (i as f32 * bucket) as usize;
            let hi = (((i + 1) as f32 * bucket) as usize).min(samples.len());
            if lo >= hi {
                0.0
            } else {
                samples[lo..hi].iter().cloned().fold(0.0f32, f32::max)
            }
        })
        .collect()
}

fn compute_total_spectrum(entries: &[UIEntry]) -> Vec<f32> {
    let mut total = vec![0.0f32; 401];
    for entry in entries {
        if let UIEntry::Emitter {
            flux,
            spectrum: Some(spec),
            source_entry_index,
            ..
        } = entry
        {
            if *flux > 0.0 {
                let source_flux = entries[*source_entry_index].flux();
                if source_flux > 0.0 {
                    let scale = *flux * source_flux;
                    for (i, &v) in spec.iter().enumerate() {
                        if i < total.len() {
                            total[i] += v * scale;
                        }
                    }
                }
            }
        }
    }
    total
}

fn compute_source_spectrum(entries: &[UIEntry], source_entry_idx: usize) -> Vec<f32> {
    let source_flux = entries[source_entry_idx].flux();
    let mut total = vec![0.0f32; 401];
    for entry in entries {
        if let UIEntry::Emitter {
            flux,
            spectrum: Some(spec),
            source_entry_index,
            ..
        } = entry
        {
            if *source_entry_index == source_entry_idx && *flux > 0.0 && source_flux > 0.0 {
                let scale = *flux * source_flux;
                for (i, &v) in spec.iter().enumerate() {
                    if i < total.len() {
                        total[i] += v * scale;
                    }
                }
            }
        }
    }
    total
}

fn wavelength_color(nm: f32) -> &'static str {
    if nm < 450.0 {
        "violet"
    } else if nm < 480.0 {
        "blue"
    } else if nm < 500.0 {
        "cyan"
    } else if nm < 530.0 {
        "green"
    } else if nm < 570.0 {
        "lime"
    } else if nm < 600.0 {
        "amber"
    } else if nm < 640.0 {
        "orange"
    } else {
        "red"
    }
}

fn w(stdout: &mut io::Stdout, s: &str) -> Result<(), enody::Error> {
    write!(stdout, "{}", s).map_err(to_err)
}

fn to_err(e: io::Error) -> enody::Error {
    enody::Error::Debug(e.to_string())
}
