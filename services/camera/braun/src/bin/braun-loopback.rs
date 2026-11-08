//! braun-loopback: braun's cameras as V4L2 capture devices, for apps that
//! read a webcam, such as Android's external camera HAL.
//!
//! Each `[[camera]]` in the configuration names a v4l2loopback device. The
//! device carries MJPEG at a fixed size, which braun-loopback sets and the
//! kernel keeps, so readers find the format whether or not anyone watches.
//! A camera opens through braun only while a reader streams, and closes when
//! the last one stops; v4l2loopback tells which with its client usage event.
//!
//! braun's previews are upright. A device can carry them turned the way a
//! phone's sensor sits instead, as Android expects (`rotation`, the degrees
//! the frame needs clockwise to stand upright), and a front camera's preview
//! is unmirrored, as Android expects too. A camera braun refuses, such as one
//! the user switched off, streams black.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use braun::client::{self, Session};
use braun::proto::{Reply, Request};
use braun::v4l2::{self, iow, iowr};
use jpeg_encoder::{ColorType, Encoder};
use serde::Deserialize;

const DEFAULT_CONFIG: &str = "/etc/braun/loopback.toml";
const QUALITY: u8 = 85;
/// How often a refused camera's black frame repeats.
const BLACK_INTERVAL: Duration = Duration::from_millis(100);
/// How long to wait for a missing device or braun before trying again.
const RETRY: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    camera: Vec<Loopback>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Loopback {
    /// The v4l2loopback device.
    device: PathBuf,
    /// Which camera: front or back, braun's main one where there are more.
    facing: String,
    /// The frame the device carries.
    width: u32,
    height: u32,
    /// Degrees (0, 90, 180 or 270) the device's frame needs clockwise to
    /// stand upright.
    #[serde(default)]
    rotation: u32,
    #[serde(default = "default_fps")]
    fps: u32,
}

fn default_fps() -> u32 {
    30
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = match args.as_slice() {
        [] => PathBuf::from(DEFAULT_CONFIG),
        [flag, path] if flag == "--config" => PathBuf::from(path),
        _ => {
            eprintln!("usage: braun-loopback [--config FILE]");
            return ExitCode::FAILURE;
        }
    };
    let config: Config = match std::fs::read_to_string(&path)
        .map_err(|error| error.to_string())
        .and_then(|text| toml::from_str(&text).map_err(|error| error.to_string()))
    {
        Ok(config) => config,
        Err(error) => {
            eprintln!("braun-loopback: {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    for loopback in &config.camera {
        if ![0, 90, 180, 270].contains(&loopback.rotation) || loopback.fps == 0 {
            eprintln!(
                "braun-loopback: {}: rotation must be 0, 90, 180 or 270 and fps above 0",
                loopback.device.display()
            );
            return ExitCode::FAILURE;
        }
    }
    let threads: Vec<_> = config
        .camera
        .into_iter()
        .map(|loopback| std::thread::spawn(move || serve(&loopback)))
        .collect();
    for thread in threads {
        let _ = thread.join();
    }
    ExitCode::FAILURE
}

/// Keeps `loopback`'s device fed for as long as the program runs.
fn serve(loopback: &Loopback) {
    let name = loopback.device.display().to_string();
    loop {
        if let Err(error) = run(loopback) {
            eprintln!("braun-loopback: {name}: {error}");
        }
        std::thread::sleep(RETRY);
    }
}

fn run(loopback: &Loopback) -> io::Result<()> {
    let mut device = Device::open(&loopback.device, loopback)?;
    let mut feed = Feed::new(loopback)?;
    // A first frame holds the device's output: until a writer has one,
    // readers find no format.
    device.write(&feed.black)?;
    loop {
        if device.wait_usage(None)? {
            stream(&mut device, &mut feed, loopback)?;
        }
    }
}

/// Feeds the device while a reader streams.
fn stream(device: &mut Device, feed: &mut Feed, loopback: &Loopback) -> io::Result<()> {
    let mut session = match open_camera(&loopback.facing) {
        Ok(session) => Some(session),
        Err(error) => {
            eprintln!("braun-loopback: {}: {error}", loopback.device.display());
            None
        }
    };
    let mut last_black = Instant::now() - BLACK_INTERVAL;
    loop {
        let Some(open) = session.as_mut() else {
            // No camera: black, until the readers go.
            if last_black.elapsed() >= BLACK_INTERVAL {
                device.write(&feed.black)?;
                last_black = Instant::now();
            }
            if !device.wait_usage(Some(BLACK_INTERVAL))? {
                return Ok(());
            }
            continue;
        };
        let mut fds = [
            libc::pollfd {
                fd: open.fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: device.file.as_raw_fd(),
                events: libc::POLLPRI,
                revents: 0,
            },
        ];
        poll(&mut fds, None)?;
        if fds[1].revents & libc::POLLPRI != 0 && !device.usage()? {
            let _ = open.send(&Request::Close, &[]);
            return Ok(());
        }
        if fds[0].revents == 0 {
            continue;
        }
        match open.next(false) {
            Ok(Some(Reply::Frame {
                slot,
                width,
                height,
                ..
            })) => {
                let turned = open
                    .frame(slot, width, height)
                    .map(|bgra| feed.turn(bgra, width, height));
                let _ = open.send(&Request::Release(slot), &[]);
                if turned.is_some() {
                    let jpeg = feed.encode()?;
                    device.write(&jpeg)?;
                }
            }
            Ok(Some(Reply::Failed(0, why))) => {
                eprintln!("braun-loopback: {}: {why}", loopback.device.display());
                session = None;
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("braun-loopback: {}: {error}", loopback.device.display());
                session = None;
            }
        }
    }
}

/// A session with braun, with the camera facing `facing` open.
fn open_camera(facing: &str) -> io::Result<Session> {
    let cameras = client::cameras().map_err(io::Error::other)?;
    let camera = cameras
        .iter()
        .filter(|camera| camera.facing == facing)
        .max_by_key(|camera| camera.role == "main")
        .ok_or_else(|| io::Error::other(format!("braun has no {facing} camera")))?;
    let session = Session::connect().map_err(io::Error::other)?;
    session.send(&Request::Open(camera.id.clone()), &[])?;
    Ok(session)
}

fn poll(fds: &mut [libc::pollfd], timeout: Option<Duration>) -> io::Result<usize> {
    let timeout = timeout.map_or(-1, |timeout| {
        i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX)
    });
    loop {
        // SAFETY: `fds` is a live array of pollfd for the whole call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if ready >= 0 {
            return Ok(ready as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// The frame a device carries, and how a preview becomes it.
struct Feed {
    width: u32,
    height: u32,
    rotation: u32,
    mirrored: bool,
    /// For each pixel of the device's frame, the preview pixel it shows,
    /// for previews of `map_for`'s size.
    map: Vec<u32>,
    map_for: (u32, u32),
    frame: Vec<u8>,
    black: Vec<u8>,
}

impl Feed {
    fn new(loopback: &Loopback) -> io::Result<Self> {
        let pixels = loopback.width as usize * loopback.height as usize;
        let mut feed = Self {
            width: loopback.width,
            height: loopback.height,
            rotation: loopback.rotation,
            // braun shows a front camera as a mirror would.
            mirrored: loopback.facing == "front",
            map: Vec::new(),
            map_for: (0, 0),
            frame: vec![0; pixels * 4],
            black: Vec::new(),
        };
        feed.black = feed.encode()?;
        Ok(feed)
    }

    /// Turns a BGRA preview into the device's frame.
    fn turn(&mut self, bgra: &[u8], width: u32, height: u32) {
        if self.map_for != (width, height) {
            self.map = source_map(
                (self.width, self.height),
                (width, height),
                self.rotation,
                self.mirrored,
            );
            self.map_for = (width, height);
        }
        for (pixel, &from) in self.frame.as_chunks_mut::<4>().0.iter_mut().zip(&self.map) {
            let from = from as usize * 4;
            pixel.copy_from_slice(&bgra[from..from + 4]);
        }
    }

    fn encode(&self) -> io::Result<Vec<u8>> {
        let mut jpeg = Vec::with_capacity(self.frame.len() / 8);
        let width = u16::try_from(self.width).map_err(io::Error::other)?;
        let height = u16::try_from(self.height).map_err(io::Error::other)?;
        Encoder::new(&mut jpeg, QUALITY)
            .encode(&self.frame, width, height, ColorType::Bgra)
            .map_err(io::Error::other)?;
        Ok(jpeg)
    }
}

/// For each pixel of a `out` frame, row by row, the pixel of an upright
/// `upright` preview it shows, once the frame is turned `rotation` degrees
/// clockwise; the preview is unmirrored first if `mirrored`. Sizes that
/// differ are scaled to fit, by the nearest pixel.
fn source_map(out: (u32, u32), upright: (u32, u32), rotation: u32, mirrored: bool) -> Vec<u32> {
    let (out_w, out_h) = (out.0 as u64, out.1 as u64);
    let (up_w, up_h) = (upright.0 as u64, upright.1 as u64);
    // The preview turned back to the frame's orientation.
    let (turned_w, turned_h) = if rotation.is_multiple_of(180) {
        (up_w, up_h)
    } else {
        (up_h, up_w)
    };
    let mut map = Vec::with_capacity((out_w * out_h) as usize);
    for y in 0..out_h {
        let ty = y * turned_h / out_h;
        for x in 0..out_w {
            let tx = x * turned_w / out_w;
            let (mut ux, uy) = match rotation {
                90 => (turned_h - 1 - ty, tx),
                180 => (turned_w - 1 - tx, turned_h - 1 - ty),
                270 => (ty, turned_w - 1 - tx),
                _ => (tx, ty),
            };
            if mirrored {
                ux = up_w - 1 - ux;
            }
            map.push((uy * up_w + ux) as u32);
        }
    }
    map
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    private: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

/// `struct v4l2_format` for a single-planar buffer type; the kernel's union
/// is 200 bytes and 8-byte aligned, after the type.
#[repr(C)]
struct Format {
    kind: u32,
    _pad: u32,
    pix: PixFormat,
    rest: [u8; 200 - size_of::<PixFormat>()],
}

#[repr(C)]
#[derive(Default)]
struct OutputParm {
    kind: u32,
    capability: u32,
    outputmode: u32,
    numerator: u32,
    denominator: u32,
    extendedmode: u32,
    writebuffers: u32,
    reserved: [u32; 4],
    rest: [[u32; 20]; 2],
}

#[repr(C)]
#[derive(Default)]
struct EventSubscription {
    kind: u32,
    id: u32,
    flags: u32,
    reserved: [u32; 5],
}

#[repr(C)]
struct Event {
    kind: u32,
    data: [u64; 8],
    pending: u32,
    sequence: u32,
    timestamp: [i64; 2],
    id: u32,
    reserved: [u32; 8],
}

const _: () = assert!(size_of::<Format>() == 208 && size_of::<OutputParm>() == 204);
const _: () = assert!(size_of::<Event>() == 136 && size_of::<EventSubscription>() == 32);

const BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
const PIX_FMT_MJPEG: u32 = u32::from_le_bytes(*b"MJPG");
const COLORSPACE_JPEG: u32 = 7;
const CAP_TIMEPERFRAME: u32 = 0x1000;
const VIDIOC_S_FMT: libc::c_ulong = iowr(b'V', 5, size_of::<Format>());
const VIDIOC_S_PARM: libc::c_ulong = iowr(b'V', 22, size_of::<OutputParm>());
const VIDIOC_DQEVENT: libc::c_ulong =
    (2 << 30) | ((size_of::<Event>() as libc::c_ulong) << 16) | ((b'V' as libc::c_ulong) << 8) | 89;
const VIDIOC_SUBSCRIBE_EVENT: libc::c_ulong = iow(b'V', 90, size_of::<EventSubscription>());
const EVENT_SUB_FL_SEND_INITIAL: u32 = 1;
/// v4l2loopback's events: how many readers stream, and its controls.
const EVENT_CLIENT_USAGE: u32 = 0x0800_0000 + 0x08e0_0000 + 1;
const CID_KEEP_FORMAT: u32 = 0x0098_0900 | 0xf000;

/// A v4l2loopback device, written to.
struct Device {
    file: File,
    used: bool,
}

impl Device {
    fn open(path: &Path, loopback: &Loopback) -> io::Result<Self> {
        let file = v4l2::open(path)?;
        let mut format = Format {
            kind: BUF_TYPE_VIDEO_OUTPUT,
            pix: PixFormat {
                width: loopback.width,
                height: loopback.height,
                pixelformat: PIX_FMT_MJPEG,
                field: v4l2::FIELD_NONE,
                // Room for any JPEG of the frame.
                sizeimage: loopback.width * loopback.height * 2,
                colorspace: COLORSPACE_JPEG,
                ..PixFormat::default()
            },
            _pad: 0,
            rest: [0; 200 - size_of::<PixFormat>()],
        };
        v4l2::ioctl(&file, VIDIOC_S_FMT, &mut format)?;
        if (format.pix.width, format.pix.height, format.pix.pixelformat)
            != (loopback.width, loopback.height, PIX_FMT_MJPEG)
        {
            return Err(io::Error::other(
                "the device keeps another format; reload v4l2loopback",
            ));
        }
        let mut parm = OutputParm {
            kind: BUF_TYPE_VIDEO_OUTPUT,
            capability: CAP_TIMEPERFRAME,
            numerator: 1,
            denominator: loopback.fps,
            ..OutputParm::default()
        };
        v4l2::ioctl(&file, VIDIOC_S_PARM, &mut parm)?;
        v4l2::set(&file, CID_KEEP_FORMAT, 1)?;
        let mut subscription = EventSubscription {
            kind: EVENT_CLIENT_USAGE,
            flags: EVENT_SUB_FL_SEND_INITIAL,
            ..EventSubscription::default()
        };
        v4l2::ioctl(&file, VIDIOC_SUBSCRIBE_EVENT, &mut subscription)?;
        Ok(Self { file, used: false })
    }

    fn write(&mut self, frame: &[u8]) -> io::Result<()> {
        (&self.file).write_all(frame)
    }

    /// Whether a reader streams, after the events waiting.
    fn usage(&mut self) -> io::Result<bool> {
        loop {
            // SAFETY: Event is plain data; zeroes are a valid value.
            let mut event: Event = unsafe { std::mem::zeroed() };
            match v4l2::ioctl(&self.file, VIDIOC_DQEVENT, &mut event) {
                Ok(()) => {
                    if event.kind == EVENT_CLIENT_USAGE {
                        self.used = event.data[0] as u32 != 0;
                    }
                    if event.pending == 0 {
                        return Ok(self.used);
                    }
                }
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                    return Ok(self.used);
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Waits up to `timeout` (or for ever) for the readers to change, and
    /// says whether one streams.
    fn wait_usage(&mut self, timeout: Option<Duration>) -> io::Result<bool> {
        let mut fds = [libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLPRI,
            revents: 0,
        }];
        if poll(&mut fds, timeout)? > 0 {
            self.usage()
        } else {
            Ok(self.used)
        }
    }
}

impl AsFd for Device {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::source_map;

    /// A 2 x 3 upright preview, its pixels numbered row by row:
    /// 0 1 / 2 3 / 4 5.
    const UPRIGHT: (u32, u32) = (2, 3);

    #[test]
    fn upright_frames_pass_through() {
        assert_eq!(source_map(UPRIGHT, UPRIGHT, 0, false), [0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_back_sensor_frame_turns_upright_clockwise() {
        // The 3 x 2 frame, turned 90 degrees clockwise, is the preview.
        // Its left column, read upward, is the preview's top row.
        assert_eq!(source_map((3, 2), UPRIGHT, 90, false), [1, 3, 5, 0, 2, 4]);
    }

    #[test]
    fn a_front_sensor_frame_turns_counterclockwise_and_unmirrors() {
        // Its right column, read downward, is the preview's top row.
        assert_eq!(source_map((3, 2), UPRIGHT, 270, false), [4, 2, 0, 5, 3, 1]);
        assert_eq!(source_map((3, 2), UPRIGHT, 270, true), [5, 3, 1, 4, 2, 0]);
    }

    #[test]
    fn sizes_scale_by_the_nearest_pixel() {
        assert_eq!(
            source_map((4, 6), UPRIGHT, 0, false),
            [
                0, 0, 1, 1, 0, 0, 1, 1, 2, 2, 3, 3, 2, 2, 3, 3, 4, 4, 5, 5, 4, 4, 5, 5
            ]
        );
    }
}
