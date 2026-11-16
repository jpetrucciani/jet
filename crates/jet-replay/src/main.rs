//! Dev tool: feed a recorded VT byte stream through jet-core and print the
//! screen as text, or render it to a PNG. Base of the corpus tests.

use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use jet_render::{Frame, Gpu, OFFSCREEN_FORMAT, Renderer};

const USAGE: &str = "usage: jet-replay <file|-> (--text | --png OUT | --bench) \
[--size COLSxROWS] [--scrollback LINES] [--font-px PX] [--gpu]";

/// Goldens are rendered at this size and padding.
const FONT_PX: f32 = 16.0;
const PADDING: u32 = 4;

type BoxError = Box<dyn std::error::Error>;

enum Output {
    Text,
    Png(String),
    Bench,
}

struct Args {
    input: String,
    output: Output,
    cols: u16,
    rows: u16,
    scrollback: usize,
    font_px: f32,
    /// Use the real GPU instead of the software fallback adapter.
    gpu: bool,
}

fn main() -> ExitCode {
    match parse_args(std::env::args().skip(1)).and_then(|args| run(&args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("jet-replay: {err}\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn parse_args(mut argv: impl Iterator<Item = String>) -> Result<Args, BoxError> {
    let (mut input, mut output) = (None, None);
    let mut args = Args {
        input: String::new(),
        output: Output::Text,
        cols: 80,
        rows: 24,
        scrollback: 0,
        font_px: FONT_PX,
        gpu: false,
    };
    while let Some(arg) = argv.next() {
        let mut value = || argv.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--text" => output = Some(Output::Text),
            "--png" => output = Some(Output::Png(value()?)),
            "--bench" => output = Some(Output::Bench),
            "--size" => (args.cols, args.rows) = parse_size(&value()?)?,
            "--scrollback" => args.scrollback = value()?.parse()?,
            "--font-px" => args.font_px = value()?.parse()?,
            "--gpu" => args.gpu = true,
            other if !other.starts_with("--") || other == "-" => input = Some(arg),
            other => return Err(format!("unknown argument {other:?}").into()),
        }
    }
    args.input = input.ok_or("an input file is required")?;
    args.output = output.ok_or("pick --text, --png OUT or --bench")?;
    Ok(args)
}

fn parse_size(size: &str) -> Result<(u16, u16), String> {
    let parsed = size
        .split_once('x')
        .and_then(|(c, r)| Some((c.parse().ok()?, r.parse().ok()?)));
    match parsed {
        Some((c, r)) if c > 0 && r > 0 => Ok((c, r)),
        _ => Err(format!("invalid --size {size:?}, expected e.g. 120x40")),
    }
}

fn run(args: &Args) -> Result<(), BoxError> {
    let bytes = if args.input == "-" {
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        buf
    } else {
        std::fs::read(&args.input).map_err(|err| format!("reading {}: {err}", args.input))?
    };
    let mut vt = jet_core::VtCore::new(args.cols, args.rows, args.scrollback)?;
    if let Output::Bench = args.output {
        return bench(&mut vt, &bytes, args.font_px);
    }
    vt.write(&bytes);
    match &args.output {
        Output::Text => {
            writeln!(std::io::stdout().lock(), "{}", vt.plain_text()?)?;
        }
        Output::Png(out) => {
            let gpu = Gpu::headless(!args.gpu)?;
            let image = render(&gpu, &mut vt, args.font_px)?;
            write_png(Path::new(out), &image)?;
            eprintln!(
                "wrote {out} ({}x{}) on {}",
                image.0,
                image.1,
                gpu.adapter.get_info().name
            );
        }
        Output::Bench => {}
    }
    Ok(())
}

/// Bytes per write, like the pane thread's PTY reads.
const BENCH_CHUNK: usize = 64 * 1024;
const REDRAWS: u32 = 500;

/// Time the CPU side of a flood: parsing in PTY-sized chunks, the snapshot
/// and frame build a changed screen costs (taken after every chunk here; the
/// app does it at most once per drawn frame), and a redraw with nothing new.
fn bench(vt: &mut jet_core::VtCore, bytes: &[u8], font_px: f32) -> Result<(), BoxError> {
    let mut fonts = jet_font::Fonts::new(font_px, &[], fallback())?;
    let mut frame = Frame::default();
    let pad = PADDING;
    let (mut parse, mut snap, mut build) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let (mut frames, mut last) = (0u32, None);
    for chunk in bytes.chunks(BENCH_CHUNK) {
        let t = Instant::now();
        vt.write(chunk);
        vt.take_effects()?;
        parse += t.elapsed();
        let t = Instant::now();
        let snapshot = vt.snapshot()?;
        snap += t.elapsed();
        if let Some(snapshot) = snapshot {
            let t = Instant::now();
            frame.build(&snapshot, &mut fonts, [pad, pad]);
            build += t.elapsed();
            frames += 1;
            last = Some(snapshot);
        }
    }
    let last = last.ok_or("stream produced no frames")?;
    let t = Instant::now();
    for _ in 0..REDRAWS {
        frame.build(&last, &mut fonts, [pad, pad]);
    }
    let redraw = t.elapsed() / REDRAWS;

    let mb = bytes.len() as f64 / f64::from(1 << 20);
    let per_frame = |d: Duration| d.as_secs_f64() * 1e6 / f64::from(frames.max(1));
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "{mb:.1} MiB: parse {:.1} MiB/s",
        mb / parse.as_secs_f64()
    )?;
    writeln!(
        out,
        "  per frame ({frames}): snapshot {:.1} us, build {:.1} us, redraw unchanged {:.1} us ({} instances)",
        per_frame(snap),
        per_frame(build),
        redraw.as_secs_f64() * 1e6,
        frame.instances()
    )?;
    Ok(())
}

/// Fonts from `JET_FALLBACK_FONTS` (the dev shell sets a fixed list), so
/// renders don't depend on what the machine has installed.
fn fallback() -> Box<dyn jet_font::Fallback> {
    Box::new(jet_font::fallback::FileFallback::from_env())
}

type Image = (u32, u32, Vec<u8>);

fn render(gpu: &Gpu, vt: &mut jet_core::VtCore, font_px: f32) -> Result<Image, BoxError> {
    let snapshot = vt.snapshot()?.ok_or("terminal produced no snapshot")?;
    let fonts = jet_font::Fonts::new(font_px, &[], fallback())?;
    let mut renderer = Renderer::new(&gpu.device, &gpu.queue, OFFSCREEN_FORMAT, fonts, PADDING);
    Ok(gpu.render_rgba(&mut renderer, &snapshot)?)
}

fn write_png(path: &Path, (width, height, rgba): &Image) -> Result<(), BoxError> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut encoder = png::Encoder::new(file, *width, *height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(rgba)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use jet_render::Gpu;

    use crate::{BoxError, FONT_PX, Image, render, write_png};

    /// Largest per-channel difference allowed between a render and its golden.
    const TOLERANCE: u8 = 3;

    fn corpus() -> Result<Vec<PathBuf>, BoxError> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus");
        let mut paths: Vec<_> = std::fs::read_dir(&dir)?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "vt"))
            .collect();
        paths.sort();
        assert!(!paths.is_empty(), "no corpus in {}", dir.display());
        Ok(paths)
    }

    fn name(path: &Path) -> &str {
        path.file_stem().and_then(|s| s.to_str()).unwrap_or("?")
    }

    /// Every `corpus/*.vt` stream, replayed at 80x24, matches its snapshot.
    #[test]
    fn corpus_text() -> Result<(), BoxError> {
        for path in corpus()? {
            let mut vt = jet_core::VtCore::new(80, 24, 0)?;
            vt.write(&std::fs::read(&path)?);
            insta::assert_snapshot!(name(&path), vt.plain_text()?);
        }
        Ok(())
    }

    fn read_png(path: &Path) -> Result<Image, BoxError> {
        let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path)?));
        let mut reader = decoder.read_info()?;
        let mut buf = vec![0; reader.output_buffer_size().ok_or("png too large")?];
        let info = reader.next_frame(&mut buf)?;
        buf.truncate(info.buffer_size());
        Ok((info.width, info.height, buf))
    }

    /// Every corpus stream renders like `goldens/<name>.png` on the software
    /// adapter. `JET_UPDATE_GOLDENS=1` rewrites them; a missing golden is
    /// written and fails the test so it gets reviewed.
    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "the goldens are lavapipe's pixels, which only the Linux dev shell has"
    )]
    fn corpus_goldens() -> Result<(), BoxError> {
        let gpu = Gpu::headless(true)?;
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let failures = root.join("../../target/golden-failures");
        let update = std::env::var_os("JET_UPDATE_GOLDENS").is_some_and(|v| v != "0");
        let mut problems = Vec::new();
        for path in corpus()? {
            let mut vt = jet_core::VtCore::new(80, 24, 0)?;
            vt.write(&std::fs::read(&path)?);
            let actual = render(&gpu, &mut vt, FONT_PX)?;
            let golden = root.join("goldens").join(format!("{}.png", name(&path)));
            if update || !golden.exists() {
                std::fs::create_dir_all(root.join("goldens"))?;
                write_png(&golden, &actual)?;
                if !update {
                    problems.push(format!("{}: new golden written, review it", name(&path)));
                }
                continue;
            }
            let expected = read_png(&golden)?;
            let bad = if (expected.0, expected.1) == (actual.0, actual.1) {
                let diff = expected.2.iter().zip(&actual.2);
                diff.filter(|(a, b)| a.abs_diff(**b) > TOLERANCE).count()
            } else {
                usize::MAX
            };
            if bad > 0 {
                std::fs::create_dir_all(&failures)?;
                let out = failures.join(format!("{}.png", name(&path)));
                write_png(&out, &actual)?;
                problems.push(format!(
                    "{}: {bad} channels off, got {}",
                    name(&path),
                    out.display()
                ));
            }
        }
        assert!(
            problems.is_empty(),
            "on {}:\n{}",
            gpu.adapter.get_info().name,
            problems.join("\n")
        );
        Ok(())
    }
}
