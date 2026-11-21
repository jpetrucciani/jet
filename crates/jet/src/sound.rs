//! Bell and notification sounds: files rodio decodes (wav, mp3, ogg,
//! flac), played on a thread of their own so the window never waits on
//! audio. Windows and macOS play through rodio; Linux through ALSA,
//! dlopened like the rest of what jet uses there, so a box without it
//! still runs jet, silently.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
#[cfg(any(windows, target_os = "macos"))]
use std::time::Duration;

use jet_config::Config;
use rodio::{Decoder, Source};

/// How long the audio device stays open after a sound, for the next one.
#[cfg(any(windows, target_os = "macos"))]
const IDLE: Duration = Duration::from_secs(30);

struct Request {
    path: PathBuf,
    volume: f32,
}

/// Plays sounds, one at a time: one asked for while another plays is
/// dropped, so a burst of bells doesn't queue up a minute of them.
#[derive(Default)]
pub struct Sounds {
    /// The audio thread, started with the first sound.
    tx: Option<Sender<Request>>,
}

impl Sounds {
    pub fn play(&mut self, path: &Path, volume: f32) {
        let request = Request {
            path: path.to_owned(),
            volume,
        };
        let tx = match &self.tx {
            Some(tx) => tx,
            None => {
                let (tx, rx) = channel();
                let started = std::thread::Builder::new()
                    .name("jet-sound".into())
                    .spawn(move || run(&rx));
                if let Err(err) = started {
                    return eprintln!("jet: starting the sound thread: {err}");
                }
                self.tx.insert(tx)
            }
        };
        if tx.send(request).is_err() {
            // The thread is gone; the next sound starts another.
            self.tx = None;
        }
    }
}

/// Why `config`'s sound files can't be played, if one can't.
pub fn problems(config: &Config) -> Option<String> {
    let problems: Vec<String> = config
        .sounds()
        .into_iter()
        .filter_map(|(key, path)| decode(path).err().map(|why| format!("{key}: {why}")))
        .collect();
    (!problems.is_empty()).then(|| problems.join("; "))
}

fn decode(path: &Path) -> Result<Decoder<std::io::BufReader<File>>, String> {
    let file = File::open(path).map_err(|e| format!("can't open {}: {e}", path.display()))?;
    Decoder::try_from(file).map_err(|e| format!("can't play {}: {e}", path.display()))
}

fn run(rx: &Receiver<Request>) {
    let mut out = Output::default();
    while let Some(request) = out.next(rx) {
        if let Err(err) = out.play(&request) {
            eprintln!("jet: playing {}: {err}", request.path.display());
        }
        // Asked for while that played: dropped.
        while rx.try_recv().is_ok() {}
    }
}

/// rodio's own output: the default device, kept open a while.
#[cfg(any(windows, target_os = "macos"))]
#[derive(Default)]
struct Output {
    device: Option<rodio::MixerDeviceSink>,
}

#[cfg(any(windows, target_os = "macos"))]
impl Output {
    /// The next sound to play, closing the device when none comes for a
    /// while; `None` once jet is done with sounds.
    fn next(&mut self, rx: &Receiver<Request>) -> Option<Request> {
        use std::sync::mpsc::RecvTimeoutError;
        loop {
            if self.device.is_none() {
                return rx.recv().ok();
            }
            match rx.recv_timeout(IDLE) {
                Ok(request) => return Some(request),
                Err(RecvTimeoutError::Timeout) => self.device = None,
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    fn play(&mut self, request: &Request) -> Result<(), String> {
        let source = decode(&request.path)?;
        let device = match &mut self.device {
            Some(device) => device,
            None => {
                let mut device =
                    rodio::DeviceSinkBuilder::open_default_sink().map_err(|e| e.to_string())?;
                device.log_on_drop(false);
                self.device.insert(device)
            }
        };
        let player = rodio::Player::connect_new(device.mixer());
        player.append(source.amplify(request.volume));
        player.sleep_until_end();
        Ok(())
    }
}

/// ALSA, loaded with the first sound.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct Output {
    alsa: Option<Result<alsa::Alsa, String>>,
}

#[cfg(target_os = "linux")]
impl Output {
    fn next(&mut self, rx: &Receiver<Request>) -> Option<Request> {
        rx.recv().ok()
    }

    fn play(&mut self, request: &Request) -> Result<(), String> {
        let source = decode(&request.path)?.amplify(request.volume);
        let alsa = self.alsa.get_or_insert_with(alsa::Alsa::load);
        let alsa = alsa.as_ref().map_err(Clone::clone)?;
        let channels = source.channels().get();
        let pcm = alsa.open(u32::from(channels), source.sample_rate().get())?;
        // A tenth of a second at a time.
        let chunk = usize::from(channels) * 4800;
        let mut samples = source;
        let mut buf = Vec::with_capacity(chunk);
        loop {
            buf.clear();
            buf.extend(samples.by_ref().take(chunk));
            if buf.is_empty() {
                break;
            }
            pcm.write(&buf, channels)?;
        }
        pcm.drain();
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod alsa {
    //! The few calls of libasound's simple PCM API that playing a sound
    //! takes, looked up at run time.

    use std::ffi::{CStr, c_char, c_int, c_long, c_uint, c_ulong, c_void};

    /// snd_pcm_t, only ever behind a pointer.
    #[repr(C)]
    pub struct Pcm {
        _private: [u8; 0],
    }

    const STREAM_PLAYBACK: c_int = 0;
    const FORMAT_FLOAT_LE: c_int = 14;
    const ACCESS_RW_INTERLEAVED: c_int = 3;
    /// What ALSA may buffer, in microseconds.
    const LATENCY_US: c_uint = 100_000;

    type Open = unsafe extern "C" fn(*mut *mut Pcm, *const c_char, c_int, c_int) -> c_int;
    type SetParams =
        unsafe extern "C" fn(*mut Pcm, c_int, c_int, c_uint, c_uint, c_int, c_uint) -> c_int;
    type Writei = unsafe extern "C" fn(*mut Pcm, *const c_void, c_ulong) -> c_long;
    type Recover = unsafe extern "C" fn(*mut Pcm, c_int, c_int) -> c_int;
    type Done = unsafe extern "C" fn(*mut Pcm) -> c_int;
    type StrError = unsafe extern "C" fn(c_int) -> *const c_char;

    pub struct Alsa {
        open: Open,
        set_params: SetParams,
        writei: Writei,
        recover: Recover,
        drain: Done,
        close: Done,
        strerror: StrError,
    }

    impl Alsa {
        pub fn load() -> Result<Self, String> {
            // SAFETY: a library name and flags; the handle is kept for the
            // process's life (never closed), so the symbols stay valid.
            let lib = unsafe {
                libc::dlopen(
                    c"libasound.so.2".as_ptr(),
                    libc::RTLD_NOW | libc::RTLD_LOCAL,
                )
            };
            if lib.is_null() {
                return Err("no libasound.so.2 (ALSA) to play sounds with".into());
            }
            let sym = |name: &CStr| {
                // SAFETY: a symbol name in the library just opened.
                let ptr = unsafe { libc::dlsym(lib, name.as_ptr()) };
                if ptr.is_null() {
                    Err(format!("libasound has no {}", name.to_string_lossy()))
                } else {
                    Ok(ptr)
                }
            };
            // SAFETY: each symbol has the C signature it's cast to, per
            // alsa/pcm.h and alsa/error.h.
            unsafe {
                Ok(Self {
                    open: std::mem::transmute::<*mut c_void, Open>(sym(c"snd_pcm_open")?),
                    set_params: std::mem::transmute::<*mut c_void, SetParams>(sym(
                        c"snd_pcm_set_params",
                    )?),
                    writei: std::mem::transmute::<*mut c_void, Writei>(sym(c"snd_pcm_writei")?),
                    recover: std::mem::transmute::<*mut c_void, Recover>(sym(c"snd_pcm_recover")?),
                    drain: std::mem::transmute::<*mut c_void, Done>(sym(c"snd_pcm_drain")?),
                    close: std::mem::transmute::<*mut c_void, Done>(sym(c"snd_pcm_close")?),
                    strerror: std::mem::transmute::<*mut c_void, StrError>(sym(c"snd_strerror")?),
                })
            }
        }

        fn error(&self, what: &str, code: c_int) -> String {
            // SAFETY: snd_strerror returns a static string for any code.
            let text = unsafe { CStr::from_ptr((self.strerror)(code)) };
            format!("{what}: {}", text.to_string_lossy())
        }

        /// The default device, for interleaved f32 samples.
        pub fn open(&self, channels: c_uint, rate: c_uint) -> Result<Device<'_>, String> {
            let mut pcm = std::ptr::null_mut();
            // SAFETY: an out pointer and a device name, per snd_pcm_open.
            let err = unsafe { (self.open)(&mut pcm, c"default".as_ptr(), STREAM_PLAYBACK, 0) };
            if err < 0 {
                return Err(self.error("opening the sound device", err));
            }
            let device = Device { alsa: self, pcm };
            let (format, access) = (FORMAT_FLOAT_LE, ACCESS_RW_INTERLEAVED);
            // SAFETY: the device just opened; resampling allowed.
            let err =
                unsafe { (self.set_params)(pcm, format, access, channels, rate, 1, LATENCY_US) };
            if err < 0 {
                return Err(self.error("setting up the sound device", err));
            }
            Ok(device)
        }
    }

    /// An open playback device; closed when dropped.
    pub struct Device<'a> {
        alsa: &'a Alsa,
        pcm: *mut Pcm,
    }

    impl Device<'_> {
        /// Interleaved samples, `channels` to a frame; all of them, unless
        /// the device fails.
        pub fn write(&self, samples: &[f32], channels: u16) -> Result<(), String> {
            let channels = usize::from(channels).max(1);
            let mut rest = samples;
            while rest.len() >= channels {
                let frames = c_ulong::try_from(rest.len() / channels).unwrap_or(0);
                // SAFETY: `frames` whole frames of f32s from `rest`.
                let wrote = unsafe { (self.alsa.writei)(self.pcm, rest.as_ptr().cast(), frames) };
                match usize::try_from(wrote) {
                    Ok(frames) => rest = rest.get(frames * channels..).unwrap_or(&[]),
                    Err(_) => {
                        let code = c_int::try_from(wrote).unwrap_or(c_int::MIN);
                        // An underrun or a suspend: start again from here.
                        // SAFETY: the open device and the error it gave.
                        if unsafe { (self.alsa.recover)(self.pcm, code, 1) } < 0 {
                            return Err(self.alsa.error("playing", code));
                        }
                    }
                }
            }
            Ok(())
        }

        /// Play out what's buffered.
        pub fn drain(&self) {
            // SAFETY: the open device.
            let _ = unsafe { (self.alsa.drain)(self.pcm) };
        }
    }

    impl Drop for Device<'_> {
        fn drop(&mut self) {
            // SAFETY: opened by `Alsa::open`, closed once here.
            let _ = unsafe { (self.alsa.close)(self.pcm) };
        }
    }
}

#[cfg(test)]
mod tests {
    /// A tenth of a second of 16-bit mono silence, as a WAV file.
    fn wav() -> Vec<u8> {
        let (rate, samples) = (8000u32, 800u32);
        let data = samples * 2;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data.to_le_bytes());
        out.resize(out.len() + data as usize, 0);
        out
    }

    #[test]
    fn sounds_that_wont_play_are_problems() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("jet-sound-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let (good, bad) = (dir.join("ding.wav"), dir.join("notes.txt"));
        std::fs::write(&good, wav())?;
        std::fs::write(&bad, "not a sound")?;
        let toml = format!(
            "[terminal]\nbell-sound = {:?}\n[profiles.p]\nbell-sound = {:?}\n",
            good.display().to_string(),
            bad.display().to_string()
        );
        let config = jet_config::Config::parse(&toml, &jet_config::Dirs::default())?.config;
        let problem = super::problems(&config).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            problem.starts_with("profiles.p.bell-sound: can't play"),
            "{problem}"
        );
        assert!(!problem.contains("terminal.bell-sound"), "{problem}");
        Ok(())
    }
}
