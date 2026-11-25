//! Linux: audio decoded in Rust (`foreign.rs`: symphonia, and opuscule for Opus) and written to
//! ALSA's default device, which PipeWire or PulseAudio serves on a desktop. libasound is opened
//! at run time, so a machine without it fails the clip rather than the app at launch. Video
//! draws the neutral placeholder.
//!
//! One worker thread per clip decodes it whole, then follows the handle's intent: it writes
//! the next ~20 ms while playing and folds the heard position into the state, and once paused
//! for a while it exits, freeing the samples. A rate other than 1 resamples, so pitch moves.
use super::*;
use std::ffi::{CStr, c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::io::Read;
use std::sync::{Condvar, OnceLock};
use std::time::Duration;

const VIDEO: &str = "video playback isn't supported on this platform yet";
/// 30 minutes of 48 kHz stereo, as decoded 16-bit samples (about 330 MB).
const MAX_SAMPLES: usize = 30 * 60 * 48_000 * 2;
const MAX_BYTES: u64 = 256 << 20;
/// Frames written at a time: about 20 ms, so pause and seek answer quickly.
const CHUNK: usize = 1024;
/// How long a paused clip keeps its decoded samples.
const KEEP: Duration = Duration::from_secs(15);

#[derive(Default)]
pub(crate) struct Native {
    worker: Option<Arc<Bell>>,
}

/// Rings a clip's worker after an edit. A worker that retired can't be rung, so the next edit
/// starts another.
#[derive(Default)]
struct Bell {
    /// (rung, retired)
    state: Mutex<(bool, bool)>,
    ring: Condvar,
}

impl Bell {
    fn ring(&self) -> bool {
        let mut s = self.state.lock().unwrap();
        if s.1 {
            return false;
        }
        s.0 = true;
        self.ring.notify_one();
        true
    }
    fn wait(&self, timeout: Duration) -> bool {
        let s = self.state.lock().unwrap();
        let (mut s, _) = self.ring.wait_timeout_while(s, timeout, |s| !s.0).unwrap();
        std::mem::take(&mut s.0)
    }
    /// Retires unless rung meanwhile.
    fn retire(&self) -> bool {
        let mut s = self.state.lock().unwrap();
        s.1 = !s.0;
        s.1
    }
}

pub(super) fn created(shared: &Arc<Shared>) {
    if shared.kind == Kind::Video {
        shared.ctl.lock().unwrap().fail(VIDEO.into());
    }
}

pub(super) fn enable() {}

pub(super) fn probe(_: &mut Native, source: &MediaSource, headers: &[(String, String)], kind: Kind) -> Result<Meta, String> {
    match kind {
        Kind::Video => Err(VIDEO.into()),
        Kind::Audio => load(source, headers).map(|pcm| pcm.meta()),
    }
}

pub(super) fn decode(_: &mut Native, _: &MediaSource, _: &[(String, String)], _: f64) -> Result<ByteFrame, String> {
    Err(VIDEO.into())
}

pub(super) fn apply(shared: &Arc<Shared>) {
    if playback_enabled() && shared.kind == Kind::Audio {
        wake(shared);
    }
}

pub(super) fn drive(shared: &Arc<Shared>) {
    apply(shared);
}

/// Reads an audio clip's duration before it plays.
pub(super) fn preload(shared: &Arc<Shared>) {
    apply(shared);
}

pub(super) fn live_time(_: &Shared) -> Option<f64> {
    None
}

pub(super) fn pull(_: &Arc<Shared>, _: &NativeSurface, _: Instant) -> bool {
    false
}

/// Rings the clip's worker, or starts one.
fn wake(shared: &Arc<Shared>) {
    let mut native = shared.native.lock().unwrap();
    if native.worker.as_ref().is_some_and(|bell| bell.ring()) || shared.ctl.lock().unwrap().failed.is_some() {
        return;
    }
    let bell = Arc::new(Bell::default());
    native.worker = Some(bell.clone());
    drop(native);
    let (clip, source, headers) = (Arc::downgrade(shared), shared.source.clone(), shared.headers.clone());
    let spawned = std::thread::Builder::new().name("rune-audio".into()).spawn(move || work(&clip, &source, &headers, &bell));
    if let Err(e) = spawned {
        fail(shared, format!("can't start audio: {e}"));
    }
}

fn fail(shared: &Shared, error: String) {
    shared.ctl.lock().unwrap().fail(error);
    shared.settle(true);
}

fn work(clip: &Weak<Shared>, source: &MediaSource, headers: &[(String, String)], bell: &Bell) {
    let loaded = load(source, headers);
    let Some(shared) = clip.upgrade() else { return };
    let pcm = match loaded {
        Ok(pcm) => pcm,
        Err(e) => return fail(&shared, e),
    };
    {
        let mut c = shared.ctl.lock().unwrap();
        if !c.opened() {
            c.open(Ok(pcm.meta()));
        }
    }
    shared.settle(true);
    let total = pcm.frames();
    let frame_at = |t: f64| ((t.max(0.0) * pcm.rate as f64) as usize).min(total);
    let mut at = frame_at(shared.ctl.lock().unwrap().time);
    drop(shared);
    let silence = vec![0i16; CHUNK * pcm.channels];
    // The device and the rate it was opened for.
    let mut out: Option<(Device, f32)> = None;
    let mut idle = Duration::ZERO;
    loop {
        let Some(shared) = clip.upgrade() else { return };
        let (playing, seek, rate, muted, looping) = {
            let mut c = shared.ctl.lock().unwrap();
            if c.failed.is_some() {
                return;
            }
            (c.playing, c.seek.take(), c.rate, c.muted, c.looping)
        };
        if let Some(t) = seek {
            at = frame_at(t);
            if let Some((device, _)) = &out {
                device.flush();
            }
        }
        if !playing || out.as_ref().is_some_and(|(_, r)| *r != rate) {
            if let Some((device, _)) = out.take() {
                at = at.saturating_sub(device.delay());
                if !playing {
                    shared.ctl.lock().unwrap().observe(at as f64 / pcm.rate as f64, false);
                }
            }
        }
        if !playing {
            drop(shared);
            if bell.wait(Duration::from_millis(500)) {
                idle = Duration::ZERO;
            } else {
                idle += Duration::from_millis(500);
                if idle >= KEEP && bell.retire() {
                    return;
                }
            }
            continue;
        }
        idle = Duration::ZERO;
        if out.is_none() {
            match Device::open(pcm.channels, (pcm.rate as f32 * rate).round().max(1.0) as u32) {
                Ok(device) => out = Some((device, rate)),
                Err(e) => return fail(&shared, e),
            }
        }
        let (device, _) = out.as_mut().unwrap();
        if at < total {
            let n = CHUNK.min(total - at);
            let ch = pcm.channels;
            let chunk = if muted { &silence[..n * ch] } else { &pcm.samples[at * ch..(at + n) * ch] };
            match device.write(chunk) {
                Ok(written) => at += written,
                Err(e) => return fail(&shared, e),
            }
        } else if looping {
            at = 0;
            continue;
        } else {
            // What the device still holds plays out.
            std::thread::sleep(Duration::from_millis(10));
        }
        let delay = device.delay();
        let heard = at.saturating_sub(delay) as f64 / pcm.rate as f64;
        let changed = {
            let mut c = shared.ctl.lock().unwrap();
            let generation = c.generation;
            // An edit made during the write wins.
            if c.playing && c.seek.is_none() {
                c.observe(heard, at >= total && delay == 0);
            }
            c.generation != generation
        };
        if changed {
            shared.settle(true);
        }
    }
}

struct Pcm {
    samples: Vec<i16>,
    rate: u32,
    channels: usize,
}

impl Pcm {
    fn frames(&self) -> usize {
        self.samples.len() / self.channels
    }
    fn meta(&self) -> Meta {
        Meta { duration: self.frames() as f64 / self.rate as f64, size: None, frame: 1.0 / 30.0 }
    }
}

/// A clip's bytes, decoded whole.
fn load(source: &MediaSource, headers: &[(String, String)]) -> Result<Pcm, String> {
    let mut samples = Vec::new();
    let format = super::foreign::decode(fetch(source, headers)?, &mut |chunk| {
        if samples.len() + chunk.len() > MAX_SAMPLES {
            return Err("too long to play (over 30 minutes)".into());
        }
        samples.extend_from_slice(chunk);
        Ok(())
    })?;
    if let Some(keep) = format.keep {
        samples.truncate(keep as usize);
    }
    let channels = format.channels as usize;
    if channels == 0 || format.rate == 0 || samples.len() < channels {
        return Err("no audio decoded".into());
    }
    samples.truncate(samples.len() / channels * channels);
    Ok(Pcm { samples, rate: format.rate, channels })
}

/// A file read, or a blocking fetch with the clip's request headers.
fn fetch(source: &MediaSource, headers: &[(String, String)]) -> Result<Vec<u8>, String> {
    let path = match source {
        MediaSource::File(path) => path.clone(),
        MediaSource::Url(url) if url.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("file://")) => PathBuf::from(&url[7..]),
        MediaSource::Url(url) => {
            let mut request = ureq::get(url);
            for (name, value) in headers {
                request = request.set(name, value);
            }
            let response = request.call().map_err(|e| match e {
                ureq::Error::Status(code, _) => format!("HTTP {code}"),
                ureq::Error::Transport(t) => format!("can't fetch the clip: {}", t.kind()),
            })?;
            let mut bytes = Vec::new();
            response.into_reader().take(MAX_BYTES).read_to_end(&mut bytes).map_err(|e| format!("can't fetch the clip: {e}"))?;
            return Ok(bytes);
        }
    };
    std::fs::read(&path).map_err(|e| format!("can't read {}: {e}", path.display()))
}

/// The few libasound calls a blocking writer needs.
struct Alsa {
    open: unsafe extern "C" fn(*mut *mut c_void, *const c_char, c_int, c_int) -> c_int,
    set_params: unsafe extern "C" fn(*mut c_void, c_int, c_int, c_uint, c_uint, c_int, c_uint) -> c_int,
    prepare: unsafe extern "C" fn(*mut c_void) -> c_int,
    writei: unsafe extern "C" fn(*mut c_void, *const c_void, c_ulong) -> c_long,
    recover: unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_int,
    delay: unsafe extern "C" fn(*mut c_void, *mut c_long) -> c_int,
    drop: unsafe extern "C" fn(*mut c_void) -> c_int,
    close: unsafe extern "C" fn(*mut c_void) -> c_int,
    strerror: unsafe extern "C" fn(c_int) -> *const c_char,
    _lib: libloading::Library,
}

fn alsa() -> Result<&'static Alsa, String> {
    static ALSA: OnceLock<Result<Alsa, String>> = OnceLock::new();
    ALSA.get_or_init(|| {
        let missing = |e: libloading::Error| format!("no audio output (libasound.so.2): {e}");
        // SAFETY: libasound's initializers have no preconditions, and each symbol below is
        // typed as alsa/pcm.h declares it.
        unsafe {
            let lib = libloading::Library::new("libasound.so.2").map_err(missing)?;
            unsafe fn sym<T: Copy>(lib: &libloading::Library, name: &[u8]) -> Result<T, libloading::Error> {
                unsafe { lib.get::<T>(name).map(|s| *s) }
            }
            Ok(Alsa {
                open: sym(&lib, b"snd_pcm_open\0").map_err(missing)?,
                set_params: sym(&lib, b"snd_pcm_set_params\0").map_err(missing)?,
                prepare: sym(&lib, b"snd_pcm_prepare\0").map_err(missing)?,
                writei: sym(&lib, b"snd_pcm_writei\0").map_err(missing)?,
                recover: sym(&lib, b"snd_pcm_recover\0").map_err(missing)?,
                delay: sym(&lib, b"snd_pcm_delay\0").map_err(missing)?,
                drop: sym(&lib, b"snd_pcm_drop\0").map_err(missing)?,
                close: sym(&lib, b"snd_pcm_close\0").map_err(missing)?,
                strerror: sym(&lib, b"snd_strerror\0").map_err(missing)?,
                _lib: lib,
            })
        }
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// An open playback stream on ALSA's default device, closed when dropped.
struct Device {
    alsa: &'static Alsa,
    pcm: *mut c_void,
    channels: usize,
}

impl Device {
    fn open(channels: usize, rate: u32) -> Result<Self, String> {
        let alsa = alsa()?;
        let mut pcm = std::ptr::null_mut();
        // SND_PCM_STREAM_PLAYBACK, blocking.
        check(alsa, unsafe { (alsa.open)(&mut pcm, c"default".as_ptr(), 0, 0) })?;
        let device = Self { alsa, pcm, channels };
        // SND_PCM_FORMAT_S16 in native byte order, SND_PCM_ACCESS_RW_INTERLEAVED, resampled
        // when the device runs at another rate, 100 ms of latency.
        let format = if cfg!(target_endian = "little") { 2 } else { 3 };
        check(alsa, unsafe { (alsa.set_params)(pcm, format, 3, channels as c_uint, rate, 1, 100_000) })?;
        Ok(device)
    }

    /// Blocks until the device takes some of `samples`; returns the frames it took.
    fn write(&mut self, samples: &[i16]) -> Result<usize, String> {
        let frames = (samples.len() / self.channels) as c_ulong;
        let n = unsafe { (self.alsa.writei)(self.pcm, samples.as_ptr().cast(), frames) };
        if n >= 0 {
            return Ok(n as usize);
        }
        // An underrun (or a suspend) restarts the stream.
        check(self.alsa, unsafe { (self.alsa.recover)(self.pcm, n as c_int, 1) }).map(|()| 0)
    }

    /// Frames written but not yet heard.
    fn delay(&self) -> usize {
        let mut delay: c_long = 0;
        let ok = unsafe { (self.alsa.delay)(self.pcm, &mut delay) } == 0;
        if ok { delay.max(0) as usize } else { 0 }
    }

    /// Drops what the device holds, for a seek.
    fn flush(&self) {
        unsafe {
            (self.alsa.drop)(self.pcm);
            (self.alsa.prepare)(self.pcm);
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            (self.alsa.drop)(self.pcm);
            (self.alsa.close)(self.pcm);
        }
    }
}

fn check(alsa: &Alsa, code: c_int) -> Result<(), String> {
    if code >= 0 {
        return Ok(());
    }
    let reason = unsafe { CStr::from_ptr((alsa.strerror)(code)) };
    Err(format!("audio output: {}", reason.to_string_lossy()))
}
