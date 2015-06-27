//! Fast wrapper for v4l2.
//!
//! ```no_run
//! # use std::fs;
//! # use std::io::Write;
//! use rscam::{Camera, Config};
//!
//! let mut camera = Camera::new("/dev/video0").unwrap();
//!
//! camera.start(&Config {
//!     interval: (1, 30),      // 30 fps.
//!     resolution: (1280, 720),
//!     format: b"MJPG",
//!     ..Default::default()
//! }).unwrap();
//!
//! for i in 0..10 {
//!     let frame = camera.capture().unwrap();
//!     let mut file = fs::File::create(&format!("frame-{}.jpg", i)).unwrap();
//!     file.write_all(&frame[..]).unwrap();
//! }
//! ```
//!
//! The wrapper uses v4l2 (e.g. `v4l2_ioctl()` instead of `ioctl()`) until feature `no_wrapper` is
//! enabled. The feature can be useful when it's desirable to avoid dependence on *libv4l2*.

extern crate libc;

mod v4l2;

use std::convert::From;
use std::ops::Deref;
use std::os::unix::io::RawFd;
use std::slice;
use std::sync::Arc;
use std::{io, fmt, str, result};

use v4l2::MappedRegion;


pub type Result<T> = result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// I/O error when using the camera.
    Io(io::Error),
    /// Unsupported frame interval.
    BadInterval,
    /// Unsupported resolution (width and/or height).
    BadResolution,
    /// Unsupported format of pixel.
    BadFormat,
    /// Unsupported field.
    BadField
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error::Io(err)
    }
}

/// [Details](http://linuxtv.org/downloads/v4l-dvb-apis/field-order.html#v4l2-field).
#[derive(Copy, Clone)]
#[repr(C)]
pub enum Field {
    None = 1,
    Top,
    Bottom,
    Interplaced,
    SeqTB,
    SeqBT,
    Alternate,
    InterplacedTB,
    InterplacedBT
}

pub struct Config<'a> {
    /// The mix of numerator and denominator. v4l2 uses frame intervals instead of frame rates.
    /// Default is `(1, 10)`.
    pub interval: (u32, u32),
    /// Width and height of frame.
    /// Default is `(640, 480)`.
    pub resolution: (u32, u32),
    /// FourCC of format (e.g. `b"RGB3"`). Note that case matters.
    /// Default is `b"YUYV"`.
    pub format: &'a [u8],
    /// Storage method of interlaced video.
    /// Default is `Field::None` (progressive).
    pub field: Field,
    /// Number of buffers in the queue of camera.
    /// Default is `2`.
    pub nbuffers: u32
}

impl<'a> Default for Config<'a> {
    fn default() -> Config<'a> {
        Config {
            interval: (1, 10),
            resolution: (640, 480),
            format: b"YUYV",
            field: Field::None,
            nbuffers: 2
        }
    }
}

pub struct FormatInfo {
    /// FourCC of format (e.g. `b"H264"`).
    pub format: [u8; 4],
    /// Information about the format.
    pub description: String,
    /// Raw or compressed.
    pub compressed: bool,
    /// Whether it's transcoded from a different input format.
    pub emulated: bool
}

impl FormatInfo {
    fn new(fourcc: u32, desc: &[u8], flags: u32) -> FormatInfo {
        FormatInfo {
            format: [
                (fourcc >> 0 & 0xff) as u8,
                (fourcc >> 8 & 0xff) as u8,
                (fourcc >> 16 & 0xff) as u8,
                (fourcc >> 24 & 0xff) as u8
            ],

            // Instead of unstable `position_elem()`.
            description: String::from_utf8_lossy(match desc.iter().position(|&c| c == 0) {
                Some(x) => &desc[..x],
                None    => desc
            }).into_owned(),

            compressed: flags & v4l2::FMT_FLAG_COMPRESSED != 0,
            emulated: flags & v4l2::FMT_FLAG_EMULATED != 0
        }
    }

    fn fourcc(fmt: &[u8]) -> u32 {
        fmt[0] as u32 | (fmt[1] as u32) << 8 | (fmt[2] as u32) << 16 | (fmt[3] as u32) << 24
    }
}

impl fmt::Debug for FormatInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} ({}{})", str::from_utf8(self.format.as_ref()).unwrap(),
            self.description, match (self.compressed, self.emulated) {
                (true, true) => ", compressed, emulated",
                (true, false) => ", compressed",
                (false, true) => ", emulated",
                _ => ""
            })
    }
}

pub enum ResolutionInfo {
    Discretes(Vec<(u32, u32)>),
    Stepwise {
        min: (u32, u32),
        max: (u32, u32),
        step: (u32, u32)
    }
}

impl fmt::Debug for ResolutionInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ResolutionInfo::Discretes(ref d) => {
                try!(write!(f, "Discretes: {}x{}", d[0].0, d[0].1));

                for res in (&d[1..]).iter() {
                    try!(write!(f, ", {}x{}", res.0, res.1));
                }

                Ok({})
            },
            ResolutionInfo::Stepwise {min, max, step} =>
                write!(f, "Stepwise from {}x{} to {}x{} by {}x{}",
                    min.0, min.1, max.0, max.1, step.0, step.1)
        }
    }
}

pub enum IntervalInfo {
    Discretes(Vec<(u32, u32)>),
    Stepwise {
        min: (u32, u32),
        max: (u32, u32),
        step: (u32, u32)
    }
}

impl fmt::Debug for IntervalInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            IntervalInfo::Discretes(ref d) => {
                try!(write!(f, "Discretes: {}fps", d[0].1/d[0].0));

                for res in (&d[1..]).iter() {
                    try!(write!(f, ", {}fps", res.1/res.0));
                }

                Ok({})
            },
            IntervalInfo::Stepwise {min, max, step} =>
                write!(f, "Stepwise from {}fps to {}fps by {}fps",
                    max.1/max.0, min.1/min.0, step.1/step.0)
        }
    }
}

pub struct Frame {
    /// Width and height of the frame.
    pub resolution: (u32, u32),
    /// FourCC of the format.
    pub format: [u8; 4],

    region: Arc<MappedRegion>,
    length: u32,
    fd: RawFd,
    buffer: v4l2::Buffer
}

impl Deref for Frame {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.region.ptr, self.length as usize) }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        let _ = v4l2::xioctl(self.fd, v4l2::VIDIOC_QBUF, &mut self.buffer);
    }
}

#[derive(Debug, PartialEq)]
enum State {
    Idle,
    Streaming,
    Aborted
}

pub struct Camera {
    fd: RawFd,
    state: State,
    resolution: (u32, u32),
    format: [u8; 4],
    buffers: Vec<Arc<MappedRegion>>
}

impl Camera {
    pub fn new(device: &str) -> io::Result<Camera> {
        Ok(Camera {
            fd: try!(v4l2::open(device)),
            state: State::Idle,
            resolution: (0, 0),
            format: [0; 4],
            buffers: vec![]
        })
    }

    /// Get detailed info about the available formats.
    pub fn formats(&self) -> io::Result<Vec<FormatInfo>> {
        let mut formats = vec![];
        let mut fmt = v4l2::FmtDesc::new();

        while try!(v4l2::xioctl_valid(self.fd, v4l2::VIDIOC_ENUM_FMT, &mut fmt)) {
            formats.push(FormatInfo::new(fmt.pixelformat, &fmt.description, fmt.flags));
            fmt.index += 1;
        }

        Ok(formats)
    }

    /// Get detailed info about the available resolutions.
    pub fn resolutions(&self, format: &[u8]) -> Result<ResolutionInfo> {
        if format.len() != 4 {
            return Err(Error::BadFormat);
        }

        let fourcc = FormatInfo::fourcc(format);
        let mut size = v4l2::Frmsizeenum::new(fourcc);

        try!(v4l2::xioctl_valid(self.fd, v4l2::VIDIOC_ENUM_FRAMESIZES, &mut size));

        if fourcc != size.pixelformat {
            return Err(Error::BadFormat);
        }

        if size.ftype == v4l2::FRMSIZE_TYPE_DISCRETE {
            let mut discretes = vec![(size.discrete().width, size.discrete().height)];
            size.index = 1;

            while try!(v4l2::xioctl_valid(self.fd, v4l2::VIDIOC_ENUM_FRAMESIZES, &mut size)) {
                {
                    let discrete = size.discrete();
                    discretes.push((discrete.width, discrete.height));
                }
                size.index += 1;
            }

            Ok(ResolutionInfo::Discretes(discretes))
        } else {
            let sw = size.stepwise();

            Ok(ResolutionInfo::Stepwise {
                min: (sw.min_width, sw.min_height),
                max: (sw.max_width, sw.max_height),
                step: (sw.step_width, sw.step_height)
            })
        }
    }

    /// Get detailed info about the available intervals.
    pub fn intervals(&self, format: &[u8], resolution: (u32, u32)) -> Result<IntervalInfo> {
        if format.len() != 4 {
            return Err(Error::BadFormat);
        }

        let fourcc = FormatInfo::fourcc(format);
        let mut ival = v4l2::Frmivalenum::new(fourcc, resolution);

        try!(v4l2::xioctl_valid(self.fd, v4l2::VIDIOC_ENUM_FRAMEINTERVALS, &mut ival));

        if fourcc != ival.pixelformat {
            return Err(Error::BadFormat);
        }

        if resolution != (ival.width, ival.height) {
            return Err(Error::BadResolution);
        }

        if ival.ftype == v4l2::FRMIVAL_TYPE_DISCRETE {
            let mut discretes = vec![(ival.discrete().numerator, ival.discrete().denominator)];
            ival.index = 1;

            while try!(v4l2::xioctl_valid(self.fd, v4l2::VIDIOC_ENUM_FRAMEINTERVALS, &mut ival)) {
                {
                    let discrete = ival.discrete();
                    discretes.push((discrete.numerator, discrete.denominator));
                }
                ival.index += 1;
            }

            Ok(IntervalInfo::Discretes(discretes))
        } else {
            let sw = ival.stepwise();

            Ok(IntervalInfo::Stepwise {
                min: (sw.min.numerator, sw.min.denominator),
                max: (sw.max.numerator, sw.max.denominator),
                step: (sw.step.numerator, sw.step.denominator)
            })
        }
    }

    /// Start streaming.
    ///
    /// # Panics
    /// If recalled or called after `stop()`.
    pub fn start(&mut self, config: &Config) -> Result<()> {
        assert_eq!(self.state, State::Idle);

        try!(self.tune_format(config.resolution, config.format, config.field));
        try!(self.tune_stream(config.interval));
        try!(self.alloc_buffers(config.nbuffers));

        if let Err(err) = self.streamon() {
            self.free_buffers();
            return Err(Error::Io(err));
        }

        self.resolution = config.resolution;
        self.format = [config.format[0], config.format[1], config.format[2], config.format[3]];

        self.state = State::Streaming;

        Ok(())
    }

    /// Blocking request of frame.
    /// It dequeues buffer from a driver, which will be enqueueed after destructing `Frame`.
    ///
    /// # Panics
    /// If called w/o streaming.
    pub fn capture(&self) -> io::Result<Frame> {
        assert_eq!(self.state, State::Streaming);

        let mut buf = v4l2::Buffer::new();

        try!(v4l2::xioctl(self.fd, v4l2::VIDIOC_DQBUF, &mut buf));
        assert!(buf.index < self.buffers.len() as u32);

        Ok(Frame {
            resolution: self.resolution,
            format: self.format,
            region: self.buffers[buf.index as usize].clone(),
            length: buf.bytesused,
            fd: self.fd,
            buffer: buf
        })
    }

    /// Stop streaming. Otherwise it's called after destructing `Camera`.
    ///
    /// # Panics
    /// If called w/o streaming.
    pub fn stop(&mut self) -> io::Result<()> {
        assert_eq!(self.state, State::Streaming);

        try!(self.streamoff());
        self.free_buffers();

        self.state = State::Aborted;

        Ok(())
    }

    fn tune_format(&self, resolution: (u32, u32), format: &[u8], field: Field) -> Result<()> {
        if format.len() != 4 {
            return Err(Error::BadFormat);
        }

        let fourcc = FormatInfo::fourcc(format);
        let mut fmt = v4l2::Format::new(resolution, fourcc, field as u32);

        try!(v4l2::xioctl(self.fd, v4l2::VIDIOC_S_FMT, &mut fmt));

        if resolution != (fmt.fmt.width, fmt.fmt.height) {
            return Err(Error::BadResolution);
        }

        if fourcc != fmt.fmt.pixelformat {
            return Err(Error::BadFormat);
        }

        if field as u32 != fmt.fmt.field {
            return Err(Error::BadField);
        }

        Ok(())
    }

    fn tune_stream(&self, interval: (u32, u32)) -> Result<()> {
        let mut parm = v4l2::StreamParm::new(interval);

        try!(v4l2::xioctl(self.fd, v4l2::VIDIOC_S_PARM, &mut parm));
        let time = parm.parm.timeperframe;

        match (time.numerator * interval.1, time.denominator * interval.0) {
            (0, _) | (_, 0) => Err(Error::BadInterval),
            (x, y) if x != y => Err(Error::BadInterval),
            _ => Ok(())
        }
    }

    fn alloc_buffers(&mut self, nbuffers: u32) -> Result<()> {
        let mut req = v4l2::RequestBuffers::new(nbuffers);

        try!(v4l2::xioctl(self.fd, v4l2::VIDIOC_REQBUFS, &mut req));

        for i in 0..nbuffers {
            let mut buf = v4l2::Buffer::new();
            buf.index = i;
            try!(v4l2::xioctl(self.fd, v4l2::VIDIOC_QUERYBUF, &mut buf));

            let region = try!(v4l2::mmap(buf.length as usize, self.fd, buf.m));
            self.buffers.push(Arc::new(region));
        }

        Ok(())
    }

    fn free_buffers(&mut self) {
        self.buffers.clear();
    }

    fn streamon(&self) -> io::Result<()> {
        for i in 0..self.buffers.len() {
            let mut buf = v4l2::Buffer::new();
            buf.index = i as u32;

            try!(v4l2::xioctl(self.fd, v4l2::VIDIOC_QBUF, &mut buf));
        }

        let mut typ = v4l2::BUF_TYPE_VIDEO_CAPTURE;
        try!(v4l2::xioctl(self.fd, v4l2::VIDIOC_STREAMON, &mut typ));

        Ok(())
    }

    fn streamoff(&mut self) -> io::Result<()> {
        let mut typ = v4l2::BUF_TYPE_VIDEO_CAPTURE;
        try!(v4l2::xioctl(self.fd, v4l2::VIDIOC_STREAMOFF, &mut typ));

        Ok(())
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        if self.state == State::Streaming {
            let _ = self.stop();
        }

        let _ = v4l2::close(self.fd);
    }
}

/// Alias for `Camera::new()`.
pub fn new(device: &str) -> io::Result<Camera> {
    Camera::new(device)
}
