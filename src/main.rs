use gba::{bus, cpu, ppu};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use minifb::{Key, Scale, Window, WindowOptions};
use std::collections::VecDeque;
use std::env;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

mod voxel;

fn dump_frame(fb: &[u32], w: usize, h: usize, path: &str) {
    let mut out = format!("P3\n{w} {h}\n255\n");
    for px in fb {
        out += &format!("{} {} {}\n", px >> 16 & 0xFF, px >> 8 & 0xFF, px & 0xFF);
    }
    std::fs::write(path, out).unwrap();
}

fn main() -> ExitCode {
    let Some(rom_path) = env::args().nth(1) else {
        eprintln!("usage: gba <rom.gba> [--headless] [--3d]");
        return ExitCode::FAILURE;
    };
    let headless = env::args().any(|a| a == "--headless");
    let mode3d = env::args().any(|a| a == "--3d");
    let rom = std::fs::read(&rom_path).expect("failed to read ROM");
    let save_path = format!("{rom_path}.sav");
    let mut b = bus::Bus::new(rom);
    if let Ok(sav) = std::fs::read(&save_path)
        && !b.load_save(&sav)
    {
        eprintln!(
            "warning: {save_path} is {} bytes but this cartridge has {} ({}); loaded anyway",
            sav.len(),
            b.save.len(),
            b.save_type.name()
        );
    }
    eprintln!("save type: {}", b.save_type.name());
    let mut cpu = cpu::Cpu::new(b);
    cpu.bus.pal_trace = env::var("GBA_PALTRACE").is_ok();
    // --3d: per-frame layer capture + diorama renderer (native only).
    let mut capture = mode3d.then(ppu::Capture::default);
    let mut diorama = mode3d.then(voxel::Renderer::new);

    // Region-aware cycles per instruction: IWRAM runs at full speed (the
    // m4a audio mixer lives there and needs the throughput), EWRAM has mild
    // waitstates, ROM pays full waitstates. Charging ROM code ~4 also keeps
    // boot-time arrival windows close to hardware.
    fn cpi(pc: u32) -> u64 {
        match pc >> 24 {
            0x03 => 1,
            0x02 => 3,
            _ => 4,
        }
    }

    if headless {
        // Run N frames, dump the last one as PPM. GBA_INPUT holds scripted
        // key presses: "first-last:key,..." (frame ranges, inclusive start).
        let frames: u32 = env::var("GBA_FRAMES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        let script: Vec<(u32, u32, u16)> = env::var("GBA_INPUT")
            .unwrap_or_default()
            .split(',')
            .filter_map(|part| {
                let (range, key) = part.split_once(':')?;
                let (a, b) = range.split_once('-')?;
                let bit = match key {
                    "a" => 0,
                    "b" => 1,
                    "select" => 2,
                    "start" => 3,
                    "right" => 4,
                    "left" => 5,
                    "up" => 6,
                    "down" => 7,
                    "r" => 8,
                    "l" => 9,
                    _ => return None,
                };
                Some((a.parse().ok()?, b.parse().ok()?, 1u16 << bit))
            })
            .collect();
        let mut n = 0;
        let dump_every: Option<u32> = env::var("GBA_DUMP_EVERY").ok().and_then(|v| v.parse().ok());
        let dump_dir = env::var("GBA_DUMP_DIR").unwrap_or_else(|_| "filmstrip".into());
        let trace_boot = env::var("GBA_BOOTTRACE").is_ok();
        let pc_hist: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
        let capture_audio = env::var("GBA_WAV").is_ok();
        let mut captured: Vec<f32> = Vec::new();
        let brk: Option<u32> = env::var("GBA_BREAK")
            .ok()
            .and_then(|v| u32::from_str_radix(&v, 16).ok());
        let mut brk_hits = 0;
        let mut ring: VecDeque<u32> = VecDeque::new();
        while n < frames {
            if Some(cpu.regs[15]) == brk && brk_hits < 3 {
                brk_hits += 1;
                eprintln!("BREAK {:08X}: r0-r7={:08X?}", cpu.regs[15], &cpu.regs[0..8]);
            }
            let step_cycles = cpi(cpu.regs[15]);
            if trace_boot {
                ring.push_back(cpu.regs[15]);
                if ring.len() > 80 {
                    ring.pop_front();
                }
                if cpu.bus.ime_off_count >= 2 {
                    for pc in &ring {
                        eprintln!("trace {:#010X}", pc);
                    }
                    return ExitCode::SUCCESS;
                }
            }
            cpu.step();
            cpu.bus.tick(step_cycles);
            if cpu.bus.frame_ready {
                cpu.bus.frame_ready = false;
                if capture_audio {
                    captured.append(&mut cpu.bus.audio);
                    let cap = 44100 * 2 * 10;
                    if captured.len() > cap {
                        captured.drain(..captured.len() - cap);
                    }
                } else {
                    cpu.bus.audio.clear();
                }
                n += 1;
                // GBA_DUMP_EVERY=K writes frameNNNNN.ppm into GBA_DUMP_DIR so
                // one run gives a filmstrip to find where a picture goes wrong.
                if let Some(k) = dump_every
                    && k > 0
                    && n % k == 0
                {
                    let _ = std::fs::create_dir_all(&dump_dir);
                    match (&mut capture, &mut diorama) {
                        (Some(cap), Some(dio)) => {
                            cap.run(&cpu.bus.io, &cpu.bus.palette, &cpu.bus.vram, &cpu.bus.oam);
                            dio.render(cap);
                            dump_frame(
                                &dio.buffer,
                                voxel::WIDTH,
                                voxel::HEIGHT,
                                &format!("{dump_dir}/frame{n:05}.ppm"),
                            );
                        }
                        _ => dump_frame(
                            &cpu.bus.ppu.framebuffer,
                            ppu::WIDTH,
                            ppu::HEIGHT,
                            &format!("{dump_dir}/frame{n:05}.ppm"),
                        ),
                    }
                }
                let mut held = 0u16;
                for &(a, b, bits) in &script {
                    if n >= a && n < b {
                        held |= bits;
                    }
                }
                cpu.bus.keyinput = 0x3FF & !held;
            }
        }
        if trace_boot {
            let mut v: Vec<_> = pc_hist.into_iter().collect();
            v.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
            for (pc, c) in v.into_iter().take(12) {
                eprintln!("hot {:#010X} x{}", pc, c);
            }
        }
        match (&mut capture, &mut diorama) {
            (Some(cap), Some(dio)) => {
                cap.run(&cpu.bus.io, &cpu.bus.palette, &cpu.bus.vram, &cpu.bus.oam);
                dio.render(cap);
                dump_frame(&dio.buffer, voxel::WIDTH, voxel::HEIGHT, "frame.ppm");
            }
            _ => dump_frame(&cpu.bus.ppu.framebuffer, ppu::WIDTH, ppu::HEIGHT, "frame.ppm"),
        }
        if capture_audio {
            let raw: Vec<u8> = captured.iter().flat_map(|s| s.to_le_bytes()).collect();
            std::fs::write("samples.raw", raw).unwrap();
        }
        eprintln!("pc={:#010X}", cpu.regs[15]);
        let io = &cpu.bus.io;
        let r16 = |o: usize| u16::from_le_bytes([io[o], io[o + 1]]);
        eprintln!(
            "DISPCNT={:04X} BG0CNT={:04X} BG1CNT={:04X} BG2CNT={:04X} BG3CNT={:04X}",
            r16(0),
            r16(8),
            r16(0xA),
            r16(0xC),
            r16(0xE)
        );
        eprintln!(
            "BLDCNT={:04X} BLDY={:04X} IE={:04X} IME={}",
            r16(0x50),
            r16(0x54),
            cpu.bus.ie,
            cpu.bus.ime
        );
        eprintln!(
            "DISPSTAT={:02X} IF={:04X} halted={} SIOCNT={:04X} biosflags={:08X}",
            io[4],
            cpu.bus.if_,
            cpu.halted,
            r16(0x128),
            u32::from_le_bytes([
                cpu.bus.iwram[0x7FF8],
                cpu.bus.iwram[0x7FF9],
                cpu.bus.iwram[0x7FFA],
                cpu.bus.iwram[0x7FFB]
            ])
        );
        let pal_sum: u32 = cpu.bus.palette.iter().map(|&b| b as u32).sum();
        for blk in 0..6 {
            let s: u64 = cpu.bus.vram[blk * 0x4000..(blk + 1) * 0x4000]
                .iter()
                .map(|&b| b as u64)
                .sum();
            eprintln!("vram[{blk}] sum={s}");
        }
        eprintln!("palette sum={pal_sum}");
        let written = cpu.bus.save.iter().filter(|&&b| b != 0xFF).count();
        eprintln!(
            "save: {} ({} bytes, dirty={}, {} bytes programmed)",
            cpu.bus.save_type.name(),
            cpu.bus.save.len(),
            cpu.bus.save_dirty,
            written
        );
        std::fs::write("save.bin", &cpu.bus.save).unwrap();
        std::fs::write("vram.bin", &cpu.bus.vram).unwrap();
        std::fs::write("oam.bin", cpu.bus.oam).unwrap();
        std::fs::write("io.bin", cpu.bus.io).unwrap();
        std::fs::write("pal.bin", cpu.bus.palette).unwrap();
        std::fs::write("ewram.bin", &cpu.bus.ewram).unwrap();
        std::fs::write("pal.bin", cpu.bus.palette).unwrap();
        std::fs::write("ewram.bin", &cpu.bus.ewram).unwrap();
        return ExitCode::SUCCESS;
    }

    // Audio: cpal pulls from a shared queue fed by the emulator.
    let audio_queue: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
    let stream = cpal::default_host().default_output_device().map(|dev| {
        let config = cpal::StreamConfig {
            channels: 2,
            sample_rate: 44100u32,
            buffer_size: cpal::BufferSize::Default,
        };
        let q = audio_queue.clone();
        dev.build_output_stream(
            config,
            move |out: &mut [f32], _| {
                let mut q = q.lock().unwrap();
                // All-or-nothing: partial drains crackle; silence lets the
                // queue rebuild.
                if q.len() < out.len() {
                    out.fill(0.0);
                } else {
                    for s in out.iter_mut() {
                        *s = q.pop_front().unwrap();
                    }
                }
            },
            |e| eprintln!("audio error: {e}"),
            None,
        )
        .inspect(|s| {
            s.play().ok();
        })
    });
    let audio_ok = matches!(&stream, Some(Ok(_)));

    // 3D mode renders at ~3x internally, so use a smaller window scale.
    let (win_w, win_h, scale) = if mode3d {
        (voxel::WIDTH, voxel::HEIGHT, Scale::X2)
    } else {
        (ppu::WIDTH, ppu::HEIGHT, Scale::X4)
    };
    let mut window = Window::new(
        "gba",
        win_w,
        win_h,
        WindowOptions {
            scale,
            ..Default::default()
        },
    )
    .expect("failed to open window");
    window.set_target_fps(60);

    let state_path = format!("{rom_path}.state");
    let mut frame_count = 0u64;
    let mut paused = false;
    let mut presented = vec![0u32; win_w * win_h];
    while window.is_open() && !window.is_key_down(Key::Escape) {
        // F5 save state, F7 load state, P pause, hold Tab fast-forward.
        if window.is_key_pressed(Key::F5, minifb::KeyRepeat::No) {
            match bincode::encode_to_vec(&cpu, bincode::config::standard()) {
                Ok(b) => match std::fs::write(&state_path, b) {
                    Ok(_) => eprintln!("state saved"),
                    Err(e) => eprintln!("save state failed: {e}"),
                },
                Err(e) => eprintln!("save state failed: {e}"),
            }
        }
        if window.is_key_pressed(Key::F7, minifb::KeyRepeat::No) {
            match std::fs::read(&state_path)
                .map_err(|e| e.to_string())
                .and_then(|b| {
                    bincode::decode_from_slice::<cpu::Cpu, _>(&b, bincode::config::standard())
                        .map_err(|e| e.to_string())
                }) {
                Ok((loaded, _)) => {
                    cpu = loaded;
                    eprintln!("state loaded");
                }
                Err(e) => eprintln!("load state failed: {e}"),
            }
        }
        if window.is_key_pressed(Key::P, minifb::KeyRepeat::No) {
            paused = !paused;
        }
        let turbo = window.is_key_down(Key::Tab);
        // Audio-clock pacing: emulate whole frames until the audio queue
        // holds ~100ms, so playback never starves and A/V stay locked to
        // the same clock. At most a few frames per display refresh.
        let target = 44100 * 2 / 10;
        let max_frames = if paused {
            0
        } else if turbo {
            8
        } else {
            4
        };
        for i in 0..max_frames {
            // Without audio output the queue never drains; fall back to one
            // frame per display refresh.
            if !audio_ok && i > 0 {
                break;
            }
            let queued = audio_queue.lock().unwrap().len();
            if audio_ok && !turbo && queued + cpu.bus.audio.len() >= target {
                break;
            }
            // Run to the next completed video frame so the framebuffer is
            // never presented mid-scanout (that tears during scrolling).
            let mut cycles = 0u64;
            while !cpu.bus.frame_ready && cycles < bus::CYCLES_PER_FRAME * 2 {
                let c = cpi(cpu.regs[15]);
                cpu.step();
                cpu.bus.tick(c);
                cycles += c;
            }
            cpu.bus.frame_ready = false;
            match (&mut capture, &mut diorama) {
                (Some(cap), Some(dio)) => {
                    cap.run(&cpu.bus.io, &cpu.bus.palette, &cpu.bus.vram, &cpu.bus.oam);
                    dio.render(cap);
                    presented.copy_from_slice(&dio.buffer);
                }
                _ => presented.copy_from_slice(&cpu.bus.ppu.framebuffer),
            }
        }

        // Keypad, active low: A=Z, B=X, Select=RShift, Start=Enter,
        // arrows = d-pad, L=Q, R=W. (Letters A/S are deliberately unbound:
        // players reach for "A" meaning the A button.)
        let k = |key| !window.is_key_down(key) as u16;
        cpu.bus.keyinput = k(Key::Z)
            | k(Key::X) << 1
            | k(Key::RightShift) << 2
            | k(Key::Enter) << 3
            | k(Key::Right) << 4
            | k(Key::Left) << 5
            | k(Key::Up) << 6
            | k(Key::Down) << 7
            | k(Key::W) << 8
            | k(Key::Q) << 9;

        {
            let mut q = audio_queue.lock().unwrap();
            if turbo {
                cpu.bus.audio.clear(); // keep audio realtime during fast-forward
            }
            q.extend(cpu.bus.audio.drain(..));
            // Hard cap well above the pacing target; only trims after
            // pathological pauses (window drag, sleep).
            while q.len() > 44100 {
                q.pop_front();
            }
        }

        window
            .update_with_buffer(&presented, win_w, win_h)
            .expect("window update failed");

        frame_count += 1;
        if frame_count.is_multiple_of(60) && cpu.bus.save_dirty {
            cpu.bus.save_dirty = false;
            let _ = std::fs::write(&save_path, &cpu.bus.save);
        }
    }
    if cpu.bus.save_dirty {
        let _ = std::fs::write(&save_path, &cpu.bus.save);
    }
    ExitCode::SUCCESS
}
