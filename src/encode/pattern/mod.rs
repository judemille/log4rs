//! A simple pattern-based encoder.
//!
//! The supported syntax is similar to that used by Rust's string formatting
//! infrastructure. It consists of text which will be output verbatim, with
//! formatting specifiers denoted by braces containing the configuration of the
//! formatter. This consists of a formatter name followed optionally by a
//! parenthesized argument. A subset of the standard formatting parameters is
//! also supported.
//!
//! # Supported Formatters
//!
//! * `d`, `date` - The current time. By default, the ISO 8601 format is used.
//!     A custom format may be provided in the syntax accepted by `chrono` as
//!     an argument.
//! * `f`, `file` - The source file that the log message came from.
//! * `l``, level` - The log level.
//! * `L`, `line` - The line that the log message came from.
//! * `m`, `message` - The log message.
//! * `M`, `module` - The module that the log message came from.
//! * `t`, `target` - The target of the log message.
//! * `T`, `thread` - The name of the thread that the log message came from.
//! * `n` - A newline.
//!
//! In addition, an "unnamed" formatter exists to apply parameters (see below)
//! to an entire group of formatters.
//!
//! # Supported Parameters
//!
//! Left and right alignment with a custom fill character and width is
//! supported. In addition, the "precision" parameter can be used to set a
//! maximum length for formatter output.
//!
//! # Examples
//!
//! The default pattern is `{d} {l} {t} - {m}{n}` which produces output like
//! `2016-03-20T22:22:20.644420340+00:00 INFO module::path - this is a log
//! message`.
//!
//! The pattern `{d(%Y-%m-%d %H:%M:%S)}` will output the current time with a
//! custom format looking like `2016-03-20 22:22:20`.
//!
//! The pattern `{m:>10.15}` will right-align the log message to a minimum of
//! 10 bytes, filling in with space  characters, and truncate output after 15
//! bytes. The message `hello` will therefore be displayed as
//! <code>     hello</code>, while the message `hello there, world!` will be
//! displayed as `hello there, wo`.
//!
//! The pattern `{({l} {m}):15.15}` will output the log level and message limited
//! to exactly 15 bytes, padding with space characters on the right if
//! necessary. The message `hello` and log level `INFO` will be displayed as
//! <code>INFO hello     </code>, while the message `hello, world!` and log
//! level `DEBUG` will be truncated to `DEBUG hello, wo`.

use chrono::UTC;
use log::{LogRecord, LogLevel};
use serde_value::Value;
use std::default::Default;
use std::cmp;
use std::error;
use std::fmt;
use std::fmt::Write as FmtWrite;
use std::io;
use std::io::Write;
use std::thread;

use encode::pattern::parser::{Parser, Piece, Parameters, Alignment};
use encode::{self, Encode};
use file::{Deserialize, Deserializers};
use ErrorInternals;

mod parser;

#[cfg(windows)]
const NEWLINE: &'static str = "\r\n";
#[cfg(not(windows))]
const NEWLINE: &'static str = "\n";

include!("serde.rs");

struct PrecisionWriter<'a> {
    precision: usize,
    w: &'a mut encode::Write,
}

impl<'a> io::Write for PrecisionWriter<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // we don't want to report EOF, so just act as a sink past this point
        if self.precision == 0 {
            return Ok(buf.len());
        }

        let buf = &buf[..cmp::min(buf.len(), self.precision)];
        match self.w.write(buf) {
            Ok(len) => {
                self.precision -= len;
                Ok(len)
            }
            Err(e) => Err(e),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

impl<'a> encode::Write for PrecisionWriter<'a> {}

struct LeftAlignWriter<'a> {
    width: usize,
    fill: char,
    w: PrecisionWriter<'a>,
}

impl<'a> LeftAlignWriter<'a> {
    fn finish(mut self) -> io::Result<()> {
        for _ in 0..self.width {
            try!(write!(self.w, "{}", self.fill));
        }
        Ok(())
    }
}

impl<'a> io::Write for LeftAlignWriter<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.w.write(buf) {
            Ok(len) => {
                self.width = self.width.saturating_sub(len);
                Ok(len)
            }
            Err(e) => Err(e),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

impl<'a> encode::Write for LeftAlignWriter<'a> {}

struct RightAlignWriter<'a> {
    width: usize,
    fill: char,
    w: PrecisionWriter<'a>,
    buf: Vec<u8>,
}

impl<'a> RightAlignWriter<'a> {
    fn finish(mut self) -> io::Result<()> {
        for _ in 0..self.width {
            try!(write!(self.w, "{}", self.fill));
        }
        self.w.write_all(&self.buf)
    }
}

impl<'a> io::Write for RightAlignWriter<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.width = self.width.saturating_sub(buf.len());
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> encode::Write for RightAlignWriter<'a> {}

enum Chunk {
    Text(String),
    Formatted {
        chunk: FormattedChunk,
        params: Parameters,
    },
    Error(String),
}

impl Chunk {
    fn encode(&self,
              w: &mut encode::Write,
              level: LogLevel,
              target: &str,
              location: &Location,
              args: &fmt::Arguments)
              -> io::Result<()> {
        match *self {
            Chunk::Text(ref s) => w.write_all(s.as_bytes()),
            Chunk::Formatted { ref chunk, ref params } => {
                let w = PrecisionWriter {
                    precision: params.precision,
                    w: w,
                };

                match params.align {
                    Alignment::Left => {
                        let mut w = LeftAlignWriter {
                            width: params.width,
                            fill: params.fill,
                            w: w,
                        };
                        try!(chunk.encode(&mut w, level, target, location, args));
                        w.finish()
                    }
                    Alignment::Right => {
                        let mut w = RightAlignWriter {
                            width: params.width,
                            fill: params.fill,
                            w: w,
                            buf: vec![],
                        };
                        try!(chunk.encode(&mut w, level, target, location, args));
                        w.finish()
                    }
                }
            }
            Chunk::Error(ref s) => write!(w, "{{ERROR: {}}}", s),
        }
    }
}

impl<'a> From<Piece<'a>> for Chunk {
    fn from(piece: Piece<'a>) -> Chunk {
        match piece {
            Piece::Text(text) => Chunk::Text(text.to_owned()),
            Piece::Argument { formatter, parameters } => {
                match formatter.name {
                    "d" |
                    "date" => {
                        let mut format = String::new();
                        for piece in &formatter.arg {
                            match *piece {
                                Piece::Text(text) => format.push_str(text),
                                Piece::Argument { .. } => {
                                    format.push_str("{ERROR: unexpected formatter}");
                                }
                                Piece::Error(ref err) => {
                                    format.push_str("{ERROR: ");
                                    format.push_str(err);
                                    format.push('}');
                                }
                            }
                        }
                        if format.is_empty() {
                            format.push_str("%+");
                        }
                        Chunk::Formatted {
                            chunk: FormattedChunk::Time(format),
                            params: parameters,
                        }
                    }
                    "l" |
                    "level" => no_args(&formatter.arg, parameters, FormattedChunk::Level),
                    "m" |
                    "message" => no_args(&formatter.arg, parameters, FormattedChunk::Message),
                    "M" |
                    "module" => no_args(&formatter.arg, parameters, FormattedChunk::Module),
                    "f" |
                    "file" => no_args(&formatter.arg, parameters, FormattedChunk::File),
                    "L" |
                    "line" => no_args(&formatter.arg, parameters, FormattedChunk::Line),
                    "T" |
                    "thread" => no_args(&formatter.arg, parameters, FormattedChunk::Thread),
                    "t" |
                    "target" => no_args(&formatter.arg, parameters, FormattedChunk::Target),
                    "n" => no_args(&formatter.arg, parameters, FormattedChunk::Newline),
                    "" => {
                        let chunks = formatter.arg.into_iter().map(From::from).collect();
                        Chunk::Formatted {
                            chunk: FormattedChunk::Align(chunks),
                            params: parameters,
                        }
                    }
                    name => Chunk::Error(format!("unknown formatter `{}`", name)),
                }
            }
            Piece::Error(err) => Chunk::Error(err),
        }
    }
}

enum FormattedChunk {
    Time(String),
    Level,
    Message,
    Module,
    File,
    Line,
    Thread,
    Target,
    Newline,
    Align(Vec<Chunk>),
}

impl FormattedChunk {
    fn encode(&self,
              w: &mut encode::Write,
              level: LogLevel,
              target: &str,
              location: &Location,
              args: &fmt::Arguments)
              -> io::Result<()> {
        match *self {
            FormattedChunk::Time(ref fmt) => write!(w, "{}", UTC::now().format(fmt)),
            FormattedChunk::Level => write!(w, "{}", level),
            FormattedChunk::Message => w.write_fmt(*args),
            FormattedChunk::Module => w.write_all(location.module_path.as_bytes()),
            FormattedChunk::File => w.write_all(location.file.as_bytes()),
            FormattedChunk::Line => write!(w, "{}", location.line),
            FormattedChunk::Thread => {
                w.write_all(thread::current().name().unwrap_or("<unnamed>").as_bytes())
            }
            FormattedChunk::Target => w.write_all(target.as_bytes()),
            FormattedChunk::Newline => w.write_all(NEWLINE.as_bytes()),
            FormattedChunk::Align(ref chunks) => {
                for chunk in chunks {
                    try!(chunk.encode(w, level, target, location, args));
                }
                Ok(())
            }
        }
    }
}

/// An `Encode`r configured via a format string.
pub struct PatternEncoder {
    chunks: Vec<Chunk>,
    pattern: String,
}

impl fmt::Debug for PatternEncoder {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("PatternEncoder")
           .field("pattern", &self.pattern)
           .finish()
    }
}

/// Returns a `PatternEncoder` using the default pattern of `{d} {l} {t} - {m}{n}`.
impl Default for PatternEncoder {
    fn default() -> PatternEncoder {
        PatternEncoder::new("{d} {l} {t} - {m}{n}")
    }
}

impl Encode for PatternEncoder {
    fn encode(&self, w: &mut encode::Write, record: &LogRecord) -> io::Result<()> {
        let location = Location {
            module_path: record.location().module_path(),
            file: record.location().file(),
            line: record.location().line(),
        };
        self.append_inner(w, record.level(), record.target(), &location, record.args())
    }
}

fn no_args(arg: &[Piece], params: Parameters, chunk: FormattedChunk) -> Chunk {
    if arg.is_empty() {
        Chunk::Formatted {
            chunk: chunk,
            params: params,
        }
    } else {
        Chunk::Error("unexpected arguments".to_owned())
    }
}

impl PatternEncoder {
    /// Creates a `PatternEncoder` from a pattern string.
    ///
    /// The pattern string syntax is documented in the `pattern` module.
    pub fn new(pattern: &str) -> PatternEncoder {
        PatternEncoder {
            chunks: Parser::new(pattern).map(From::from).collect(),
            pattern: pattern.to_owned(),
        }
    }

    fn append_inner(&self,
                    w: &mut encode::Write,
                    level: LogLevel,
                    target: &str,
                    location: &Location,
                    args: &fmt::Arguments)
                    -> io::Result<()> {
        for chunk in &self.chunks {
            try!(chunk.encode(w, level, target, location, args));
        }
        Ok(())
    }
}

struct Location<'a> {
    module_path: &'a str,
    file: &'a str,
    line: u32,
}

/// A deserializer for the `PatternEncoder`.
///
/// The `pattern` key is required and specifies the pattern for the encoder.
pub struct PatternEncoderDeserializer;

impl Deserialize for PatternEncoderDeserializer {
    type Trait = Encode;

    fn deserialize(&self,
                   config: Value,
                   _: &Deserializers)
                   -> Result<Box<Encode>, Box<error::Error>> {
        let config = try!(config.deserialize_into::<PatternEncoderConfig>());
        let encoder = match config.pattern {
            Some(pattern) => PatternEncoder::new(&pattern),
            None => PatternEncoder::default(),
        };
        Ok(Box::new(encoder))
    }
}

#[cfg(test)]
mod tests {
    use std::default::Default;
    use std::thread;
    use std::io::{self, Write};
    use log::LogLevel;

    use super::{PatternEncoder, Location, Chunk};
    use encode;

    static LOCATION: Location<'static> = Location {
        module_path: "path",
        file: "file",
        line: 132,
    };

    struct SimpleWriter<W>(W);

    impl<W: Write> io::Write for SimpleWriter<W> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }

    impl<W: Write> encode::Write for SimpleWriter<W> {}

    fn error_free(encoder: &PatternEncoder) -> bool {
        encoder.chunks.iter().all(|c| {
            match *c {
                Chunk::Error(_) => false,
                _ => true,
            }
        })
    }

    #[test]
    fn invalid_formatter() {
        assert!(!error_free(&PatternEncoder::new("{x}")));
    }

    #[test]
    fn unclosed_delimiter() {
        assert!(!error_free(&PatternEncoder::new("{d(%Y-%m-%d)")));
    }

    #[test]
    fn log() {
        let pw = PatternEncoder::new("{l} {m} at {M} in {f}:{L}");
        let mut buf = SimpleWriter(vec![]);
        pw.append_inner(&mut buf,
                        LogLevel::Debug,
                        "target",
                        &LOCATION,
                        &format_args!("the message"))
          .unwrap();

        assert_eq!(buf.0, &b"DEBUG the message at path in file:132"[..]);
    }

    #[test]
    fn unnamed_thread() {
        thread::spawn(|| {
            let pw = PatternEncoder::new("{T}");
            let mut buf = SimpleWriter(vec![]);
            pw.append_inner(&mut buf,
                            LogLevel::Debug,
                            "target",
                            &LOCATION,
                            &format_args!("message"))
              .unwrap();
            assert_eq!(buf.0, b"<unnamed>");
        })
            .join()
            .unwrap();
    }

    #[test]
    fn named_thread() {
        thread::Builder::new()
            .name("foobar".to_string())
            .spawn(|| {
                let pw = PatternEncoder::new("{T}");
                let mut buf = SimpleWriter(vec![]);
                pw.append_inner(&mut buf,
                                LogLevel::Debug,
                                "target",
                                &LOCATION,
                                &format_args!("message"))
                  .unwrap();
                assert_eq!(buf.0, b"foobar");
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn default_okay() {
        assert!(error_free(&PatternEncoder::default()));
    }

    #[test]
    fn left_align() {
        let pw = PatternEncoder::new("{m:~<5.6}");

        let mut buf = SimpleWriter(vec![]);
        pw.append_inner(&mut buf,
                        LogLevel::Debug,
                        "",
                        &LOCATION,
                        &format_args!("foo"))
          .unwrap();
        assert_eq!(buf.0, b"foo~~");

        buf.0.clear();
        pw.append_inner(&mut buf,
                        LogLevel::Debug,
                        "",
                        &LOCATION,
                        &format_args!("foobar!"))
          .unwrap();
        assert_eq!(buf.0, b"foobar");
    }

    #[test]
    fn right_align() {
        let pw = PatternEncoder::new("{m:~>5.6}");

        let mut buf = SimpleWriter(vec![]);
        pw.append_inner(&mut buf,
                        LogLevel::Debug,
                        "",
                        &LOCATION,
                        &format_args!("foo"))
          .unwrap();
        assert_eq!(buf.0, b"~~foo");

        buf.0.clear();
        pw.append_inner(&mut buf,
                        LogLevel::Debug,
                        "",
                        &LOCATION,
                        &format_args!("foobar!"))
          .unwrap();
        assert_eq!(buf.0, b"foobar");
    }

    #[test]
    fn left_align_formatter() {
        let pw = PatternEncoder::new("{({l} {m}):15}");

        let mut buf = SimpleWriter(vec![]);
        pw.append_inner(&mut buf,
                        LogLevel::Info,
                        "",
                        &LOCATION,
                        &format_args!("foobar!"))
          .unwrap();
        assert_eq!(buf.0, b"INFO foobar!   ");
    }

    #[test]
    fn right_align_formatter() {
        let pw = PatternEncoder::new("{({l} {m}):>15}");

        let mut buf = SimpleWriter(vec![]);
        pw.append_inner(&mut buf,
                        LogLevel::Info,
                        "",
                        &LOCATION,
                        &format_args!("foobar!"))
          .unwrap();
        assert_eq!(buf.0, b"   INFO foobar!");
    }
}