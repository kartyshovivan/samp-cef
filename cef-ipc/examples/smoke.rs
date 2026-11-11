//! Drives cef_host.exe without the game: starts it, opens a page, measures frame rate.
//!
//!   cargo run -p cef-ipc --example smoke --target i686-pc-windows-msvc -- <cef dir> [url] [seconds] [gpu]

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use cef_ipc::pipe::{self, Direction, PendingPipe};
use cef_ipc::{Command, Event, FrameBuffer, PROTOCOL_VERSION, Value};

const ANIMATION: &str = "data:text/html,<style>body{margin:0;background:%23222}\
@keyframes a{from{transform:translateX(0)}to{transform:translateX(1000px)}}\
div{width:200px;height:200px;background:%23e33;animation:a 1s linear infinite alternate}</style><div></div>";

fn main() {
    let mut args = std::env::args().skip(1);
    let cef_dir = PathBuf::from(args.next().expect("usage: smoke <cef dir> [url] [seconds]"));
    // `@file` reads the URL from a file (data: URLs can exceed the command-line limit).
    let url = match args.next() {
        Some(arg) if arg.starts_with('@') => std::fs::read_to_string(&arg[1..]).expect("read url file").trim().to_string(),
        Some(arg) => arg,
        None => ANIMATION.to_string(),
    };
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);
    let gpu = args.next().as_deref() == Some("gpu");
    // SMOKE_EXT=1: game-driven frames, a BeginFrame every ~16.7 ms like the game loop would.
    let ext_frames = std::env::var("SMOKE_EXT").is_ok();
    let width: u32 = std::env::var("SMOKE_W").ok().and_then(|v| v.parse().ok()).unwrap_or(1920);
    let height: u32 = std::env::var("SMOKE_H").ok().and_then(|v| v.parse().ok()).unwrap_or(1080);

    let base = format!("cef_smoke_{}", std::process::id());
    let (c2h_name, h2c_name) = cef_ipc::pipe_names(&base);
    let c2h = PendingPipe::create(&c2h_name, Direction::Outbound).unwrap();
    let h2c = PendingPipe::create(&h2c_name, Direction::Inbound).unwrap();

    // Same as the game: relative file:/// URLs resolve against the game folder.
    let game_dir = cef_dir.parent().unwrap_or(&cef_dir).to_path_buf();

    // The 64-bit CEF host lives in cef/host (see client/src/browser/host.rs).
    // SMOKE_LEGACY=1: the 32-bit CEF 89 host in cef/ instead.
    let host_exe = if std::env::var("SMOKE_LEGACY").is_err() && cef_dir.join("host").join("cef_host.exe").exists() {
        cef_dir.join("host").join("cef_host.exe")
    } else {
        cef_dir.join("cef_host.exe")
    };
    let mut child = std::process::Command::new(host_exe)
        .current_dir(&game_dir)
        .arg(format!("--ipc={}", base))
        .arg(format!("--cef-dir={}", cef_dir.display()))
        .arg(format!("--parent-pid={}", std::process::id()))
        .args(gpu.then_some("--gpu"))
        .spawn()
        .expect("spawn cef_host.exe");

    let started = Instant::now();
    let commands = std::sync::Arc::new(std::sync::Mutex::new(c2h.accept().unwrap()));
    let mut events = h2c.accept().unwrap();
    println!("connected in {:?}", started.elapsed());

    let send = |file: &std::sync::Arc<std::sync::Mutex<std::fs::File>>, cmd: Command| {
        pipe::write_message(&mut file.lock().unwrap(), &cmd.encode()).unwrap()
    };
    send(&commands, Command::Hello { version: PROTOCOL_VERSION });

    let (tx, rx) = mpsc::channel::<Event>();
    // SMOKE_SNAPSHOT_PNG=<file>: answer snapshot requests with that PNG (empty = 404).
    let snapshot_png: Vec<u8> = std::env::var("SMOKE_SNAPSHOT_PNG").ok().and_then(|p| std::fs::read(p).ok()).unwrap_or_default();
    let snapshot_commands = commands.clone();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        while pipe::read_message(&mut events, &mut buf).is_ok() {
            if let Ok(ev) = Event::decode(&buf) {
                if let Event::SnapshotRequest { req, .. } = &ev {
                    let answer = Command::SnapshotResult { req: *req, png: snapshot_png.clone() };
                    let _ = pipe::write_message(&mut snapshot_commands.lock().unwrap(), &answer.encode());
                }
                if tx.send(ev).is_err() {
                    break;
                }
            }
        }
    });

    let wait = |rx: &mpsc::Receiver<Event>, what: &str, pred: &dyn Fn(&Event) -> bool| -> Event {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(ev) if pred(&ev) => return ev,
                Ok(ev) => println!("  event: {:?}", short(&ev)),
                Err(_) => panic!("timed out waiting for {}", what),
            }
        }
    };

    wait(&rx, "Ready", &|e| matches!(e, Event::Ready));
    println!("CEF ready in {:?}", started.elapsed());

    send(
        &commands,
        Command::CreateBrowser { id: 1, url: url.clone(), width, height, hidden: false, audio: false, external_frames: ext_frames },
    );

    if ext_frames {
        let commands = commands.clone();
        std::thread::spawn(move || {
            let period = Duration::from_micros(16_667);
            let mut next = Instant::now();
            loop {
                next += period;
                if pipe::write_message(&mut commands.lock().unwrap(), &Command::BeginFrame { id: 1 }.encode()).is_err() {
                    break;
                }
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                }
            }
        });
    }


    let mut fb: Option<FrameBuffer> = None;
    let mut loaded = false;
    let mut created = false;
    let deadline = Instant::now() + Duration::from_secs(30);

    while !(loaded && created && fb.is_some()) {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left).expect("timed out waiting for browser") {
            Event::AfterCreated { id: 1 } => {
                created = true;
                println!("AfterCreated in {:?}", started.elapsed());
            }
            Event::LoadEnd { id: 1, status } => {
                loaded = true;
                println!("LoadEnd status {} in {:?}", status, started.elapsed());
            }
            Event::FrameBuffer { id: 1, name, capacity } => {
                println!("FrameBuffer {} ({} bytes)", name, capacity);
                fb = Some(FrameBuffer::open(&name).unwrap());
            }
            other => println!("  event: {:?}", short(&other)),
        }
    }

    let fb = fb.unwrap();

    // SMOKE_SCENARIO=event: open the event menu (UI 26) and click through its tabs.
    let scenario_event = std::env::var("SMOKE_SCENARIO").as_deref() == Ok("event");

    // Records when frames arrive (ms since start) for `dur`, running `step` every ~8 ms.
    let watch = |fb: &FrameBuffer, dur: Duration, step: &mut dyn FnMut(Duration)| -> Vec<u32> {
        let start = Instant::now();
        let mut last = fb.seq();
        let mut times = Vec::new();
        let mut next_step = Instant::now();
        while start.elapsed() < dur {
            if Instant::now() >= next_step {
                step(start.elapsed());
                next_step += Duration::from_millis(8);
            }
            let seq = fb.seq();
            if seq != last {
                last = seq;
                times.push(start.elapsed().as_millis() as u32);
                fb.lock().clear_rects();
            }
            while rx.try_recv().is_ok() {}
            std::thread::sleep(Duration::from_micros(500));
        }
        times
    };
    let max_gap = |times: &[u32], end: u32| -> u32 {
        let mut prev = 0;
        let mut gap = 0;
        for &t in times.iter().chain(std::iter::once(&end)) {
            gap = gap.max(t - prev);
            prev = t;
        }
        gap
    };

    let trace = std::env::var("SMOKE_TRACE").is_ok();

    if let Some(fps) = std::env::var("SMOKE_FPS").ok().and_then(|v| v.parse().ok()) {
        send(&commands, Command::FrameRate { id: 1, fps });
    }

    if scenario_event {
        // Let the page settle (and prewarm whatever it prewarms at load) before opening.
        watch(&fb, Duration::from_millis(3000), &mut |_| {});

        let open_data = event_data();
        let mut opened = false;
        let open = watch(&fb, Duration::from_millis(1000), &mut |_| {
            if !opened {
                opened = true;
                send(&commands, Command::TriggerEvent { id: 1, name: "26:0".into(), args: Value::List(vec![Value::String(open_data.clone())]) });
            }
        });
        let first: Vec<u32> = open.iter().copied().filter(|&t| t <= 600).collect();
        println!("open: {} frames in first 600 ms, max gap {} ms, timeline {:?}", first.len(), max_gap(&first, 600), first);

        if std::env::var("SMOKE_SCROLL").is_ok() {
            // Shop tab, then drag the cards left for 1.5 s like a player does.
            let (sx, sy) = {
                let scale = (width as f64 / 1920.0).min(height as f64 / 1080.0);
                let x = 60.0 + 228.0 + 15.0 + 299.0 + 15.0 + 114.0;
                ((width as f64 / 2.0 + (x - 960.0) * scale) as i32, (height as f64 / 2.0 + (82.0 - 540.0) * scale) as i32)
            };
            send(&commands, Command::MouseMove { id: 1, x: sx, y: sy, modifiers: 0 });
            send(&commands, Command::MouseClick { id: 1, x: sx, y: sy, modifiers: 0, button: 0, down: true });
            send(&commands, Command::MouseClick { id: 1, x: sx, y: sy, modifiers: 0, button: 0, down: false });
            watch(&fb, Duration::from_millis(1500), &mut |_| {});

            let (x0, y0) = ((width as f64 * 0.75) as i32, (height as f64 * 0.55) as i32);
            send(&commands, Command::MouseMove { id: 1, x: x0, y: y0, modifiers: 0 });
            send(&commands, Command::MouseClick { id: 1, x: x0, y: y0, modifiers: 0, button: 0, down: true });
            let drag_ms: u64 = std::env::var("SMOKE_DRAG_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(1500);
            let speed = 1500.0 / drag_ms as f64; // same distance, slower drag
            let drag = watch(&fb, Duration::from_millis(drag_ms), &mut |t| {
                let x = x0 - (t.as_millis() as f64 * speed) as i32;
                let _ = send(&commands, Command::MouseMove { id: 1, x, y: y0, modifiers: cef_ipc_left_button() });
            });
            send(&commands, Command::MouseClick { id: 1, x: x0 - 1500, y: y0, modifiers: 0, button: 0, down: false });
            println!("scroll drag: {:.1} frames/s, max gap {} ms", drag.len() as f64 * 1000.0 / drag_ms as f64, max_gap(&drag, drag_ms as u32));
            if trace {
                println!("  drag timeline: {:?}", drag);
            }
        }
    } else {
        send(
            &commands,
            Command::TriggerEvent { id: 1, name: "smoke".into(), args: Value::List(vec![Value::Int(1)]) },
        );
    }

    // Tab centres in the 1920x1080 layout, mapped to the screen like `.canvas` does.
    let scale = (width as f64 / 1920.0).min(height as f64 / 1080.0);
    let tab_widths = [228.0, 299.0, 228.0, 299.0, 299.0];
    let tab_point = |tab: usize| {
        let x = 60.0 + tab_widths[..tab].iter().map(|w| w + 15.0).sum::<f64>() + tab_widths[tab] / 2.0;
        let sx = width as f64 / 2.0 + (x - 960.0) * scale;
        let sy = height as f64 / 2.0 + (82.0 - 540.0) * scale;
        (sx as i32, sy as i32)
    };
    let switch_every = Duration::from_millis(800);
    let order = [1usize, 2, 3, 4, 0];
    let mut next_switch = Instant::now();
    let mut switch_idx = 0;
    let mut last_click: Option<Instant> = None;
    let mut pending_first: Option<(usize, Instant)> = None;
    let mut first_frame: Vec<(usize, Duration)> = Vec::new();
    let mut timeline: Vec<u32> = Vec::new();

    // Measure how often the host delivers frames, and how big the gaps are.
    let mut last_seq = fb.seq();
    let mut last_change = Instant::now();
    let mut frames = 0u32;
    let mut max_gap = Duration::ZERO;
    let measure_start = Instant::now();

    while measure_start.elapsed() < Duration::from_secs(seconds) {
        if scenario_event && Instant::now() >= next_switch {
            let (x, y) = tab_point(order[switch_idx % order.len()]);
            switch_idx += 1;
            send(&commands, Command::MouseMove { id: 1, x, y, modifiers: 0 });
            send(&commands, Command::MouseClick { id: 1, x, y, modifiers: 0, button: 0, down: true });
            send(&commands, Command::MouseClick { id: 1, x, y, modifiers: 0, button: 0, down: false });
            if trace && !timeline.is_empty() {
                println!("  frames after click (ms): {:?}", timeline);
                timeline.clear();
            }
            last_click = Some(Instant::now());
            pending_first = Some((order[(switch_idx - 1) % order.len()], Instant::now()));
            next_switch += switch_every;
        }

        let seq = fb.seq();
        if seq != last_seq {
            frames += seq.wrapping_sub(last_seq);

            if let Some((tab, at)) = pending_first.take() {
                first_frame.push((tab, at.elapsed()));
            }
            if trace {
                if let Some(click) = last_click {
                    timeline.push(click.elapsed().as_millis() as u32);
                }
            }

            // In the event scenario only gaps inside a tab transition (the 400 ms fade
            // after a click) matter; idle time between switches is expected.
            let gap = match last_click {
                Some(click) if scenario_event => {
                    if click.elapsed() < Duration::from_millis(600) {
                        last_change.max(click).elapsed()
                    } else {
                        Duration::ZERO
                    }
                }
                _ => last_change.elapsed(),
            };
            max_gap = max_gap.max(gap);
            last_change = Instant::now();
            last_seq = seq;
            fb.lock().clear_rects();
        }
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::ProcessMessage { .. } => {}
                other => println!("  event: {:?}", short(&other)),
            }
        }
        std::thread::sleep(Duration::from_micros(500));
    }

    if scenario_event {
        println!("tab switches: {}", switch_idx);
        let mut per_tab = [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        for (tab, d) in &first_frame {
            per_tab[*tab].push(d.as_millis());
        }
        println!("click -> first frame, ms (0 tasks, 1 extra, 2 shop, 3 info, 4 convert): {:?}", per_tab);
    }

    if let Ok(path) = std::env::var("SMOKE_DUMP") {
        let g = fb.lock();
        let mut out = Vec::with_capacity(8 + g.pixels().len());
        out.extend_from_slice(&g.width().to_le_bytes());
        out.extend_from_slice(&g.height().to_le_bytes());
        out.extend_from_slice(g.pixels());
        std::fs::write(&path, out).unwrap();
        println!("frame dumped to {}", path);
    }

    let (w, h, nonzero) = {
        let g = fb.lock();
        let px = g.pixels();
        (g.width(), g.height(), px.chunks_exact(4).filter(|p| p[3] != 0).count())
    };

    println!(
        "frames: {:.1}/s, max gap {:?}, frame {}x{}, opaque pixels {}",
        frames as f64 / seconds as f64,
        max_gap,
        w,
        h,
        nonzero
    );

    send(&commands, Command::Shutdown);
    let status = child.wait().unwrap();
    println!("host exited: {} after {:?}", status, started.elapsed());
}

fn short(ev: &Event) -> String {
    match ev {
        Event::PopupPaint { id, width, height, .. } => format!("PopupPaint {} {}x{}", id, width, height),
        Event::AudioPacket { id, stream, frames, .. } => format!("AudioPacket {} {} {}", id, stream, frames),
        other => format!("{:?}", other),
    }
}

/// Realistic payload for the event menu (UI 26): 5 tasks, 4 extra tasks, 24 shop items.
fn event_data() -> String {
    let task = |i: u32, s: u32| {
        format!(
            r#"{{"i":{i},"t":"ЗАДАНИЕ НОМЕР {i}","d":"Найдите 5 осколков зеркала в отмеченных зонах и верните хозяину отражения.","r":{r},"s":{s},"ut":0}}"#,
            r = 50 + i * 5
        )
    };
    let tasks: Vec<String> = (1..=5).map(|i| task(i, if i == 2 { 1 } else { 0 })).collect();
    let extra: Vec<String> = (11..=14).map(|i| task(i, 0)).collect();

    let sprites = ["sprite_acs15", "sprite_anim_bp_s6", "sprite_anim_ny26_1", "sprite_anim_ny26_2", "sprite_anim_ny26_3"];
    let items: Vec<String> = (1..=24)
        .map(|i| {
            format!(
                r#"{{"i":{i},"t":"ПРЕДМЕТ {i}","d":"Аксессуар","src":"{src}","p":{p},"c":{c},"b":1,"tm":{tm},"ct":{ct}}}"#,
                src = sprites[i as usize % sprites.len()],
                p = 1000 + i * 1500,
                c = if i % 7 == 0 { 0 } else { 10 },
                tm = if std::env::var("SMOKE_TIMERS").is_ok() && i % 3 == 0 { 7200 + i * 60 } else { 0 },
                ct = i % 7
            )
        })
        .collect();

    format!(
        r#"{{"sp":0,"b":500,"tt":43200,"ett":3600,"tl":[{}],"etl":[{}],"l":[{}],"c1":7000,"c2":1,"c3":1,"c4":1}}"#,
        tasks.join(","),
        extra.join(","),
        items.join(",")
    )
}

/// EVENTFLAG_LEFT_MOUSE_BUTTON in cef_event_flags_t.
fn cef_ipc_left_button() -> u32 {
    1 << 4
}
