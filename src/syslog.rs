//! Syslog logger. Uses the [slog-syslog] crate.
//! 
//! This module is optional; it is only available if the `syslog` feature is enabled.
//! 
//! [slog-syslog]: https://crates.io/crates/slog-syslog

#![cfg(feature = "slog-syslog")]

mod retry;

use crate::Build;
use crate::build::BuilderCommon;
use crate::error::{Error, ErrorKind};
use crate::Result;
use crate::types::{OverflowStrategy, Severity, SourceLocation};
#[cfg(feature = "slog-kvfilter")]
use crate::types::KVFilterParameters;
use dyn_clone::{clone_box, DynClone};
use retry::Retry;
use serde::{Serialize, Deserialize};
use slog::Logger;
use slog_syslog::{BasicMsgFormat3164, Facility, MsgFormat3164};
use std::borrow::Cow;
use std::fmt::Debug;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::path::Path;
use trackable::error::ErrorKindExt;

/// A logger builder which builds loggers that send log records to a syslog server.
///
/// The resulting logger will work asynchronously (the default channel size is 1024).
/// 
/// # Example
/// 
/// ```
/// use slog::info;
/// use sloggers::Build;
/// use sloggers::types::Severity;
/// use slog_syslog::Facility;
/// 
/// # fn main() -> Result<(), sloggers::Error> {
/// let logger = sloggers::syslog::SyslogBuilder::new()
///     .facility(Facility::LOG_LOCAL0)
///     .level(Severity::Debug)
///     .process_name("sloggers-example-app")
///     .build()?;
/// 
/// info!(logger, "Hello, world! This is a test message from `sloggers::syslog`.");
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct SyslogBuilder {
    common: BuilderCommon,
    facility: Option<Facility>,
    hostname: Option<Cow<'static, str>>,
    destination: Destination,
    pid: Option<u32>,
    process_name: Option<Cow<'static, str>>,
    msg_format_3164: Box<dyn MsgFormat3164CloneDebugSend>,
    deferred_error: Option<Error>,
}

impl Default for SyslogBuilder {
    fn default() -> Self {
        SyslogBuilder {
            common: BuilderCommon::default(),
            facility: None,
            hostname: None,
            destination: Destination::default(),
            pid: None,
            process_name: None,
            msg_format_3164: Box::new(BasicMsgFormat3164),
            deferred_error: None,
        }
    }
}

impl SyslogBuilder {
    /// Makes a new `SyslogBuilder` instance.
    pub fn new() -> Self {
        SyslogBuilder::default()
    }

    fn defer_error<T>(&mut self, result: Result<T>) -> Option<T> {
        match result {
            Ok(t) => Some(t),
            Err(e) => {
                self.deferred_error = Some(e);
                None
            }
        }
    }

    /// Sets the source code location type this logger will use.
    pub fn source_location(&mut self, source_location: SourceLocation) -> &mut Self {
        self.common.source_location = source_location;
        self
    }

    /// Sets the syslog facility to send logs to.
    /// 
    /// By default, this is the `user` facility.
    pub fn facility(&mut self, facility: Facility) -> &mut Self {
        self.facility = Some(facility);
        self
    }

    /// Sets the hostname that the logs are being sent from.
    /// 
    /// By default, this is the hostname of the local machine.
    pub fn hostname(&mut self, hostname: impl Into<Cow<'static, str>>) -> &mut Self {
        self.hostname = Some(hostname.into());
        self
    }

    /// Sets the overflow strategy for the logger.
    pub fn overflow_strategy(&mut self, overflow_strategy: OverflowStrategy) -> &mut Self {
        self.common.overflow_strategy = overflow_strategy;
        self
    }

    /// Sets the destination to which log records will be outputted.
    /// 
    /// The `unix`, `tcp`, and `udp` methods are convenience aliases for this method.
    /// 
    /// The default is [`Destination::Local`].
    /// 
    /// [`Destination::Local`]: enum.Destination.html#variant.Local
    pub fn destination(&mut self, destination: Destination) -> &mut Self {
        self.destination = destination;
        self
    }

    /// Send log entries to the local syslog server over a Unix-domain socket at the given path.
    pub fn unix(&mut self, socket: impl Into<Cow<'static, Path>>) -> &mut Self {
        self.destination(Destination::Unix { socket: socket.into() })
    }

    /// Send log entries over TCP to a remote syslog server.
    /// 
    /// **Warning**: Log transmission is not encrypted.
    /// 
    /// This method may block to perform a DNS lookup. If the `server` parameter resolves to more than one socket address, the first one will be used.
    pub fn tcp(&mut self, server: impl ToSocketAddrs + Debug) -> &mut Self {
        if let Some(server) = self.defer_error(lookup_one_addr(server)) {
            self.destination(Destination::Tcp { server });
        }
        self
    }

    /// Send log entries over UDP to a remote syslog server from the given local address.
    /// 
    /// **Warning**: Log transmission is not encrypted.
    /// 
    /// This method may block to perform a DNS lookup. If the `server` parameter resolves to more than one socket address, the first one will be used.
    pub fn udp_bind(&mut self, local: impl ToSocketAddrs + Debug, server: impl ToSocketAddrs + Debug) -> &mut Self {
        let local = Some(match self.defer_error(lookup_one_addr(local)) {
            Some(local) => local,
            None => return self,
        });

        let server = match self.defer_error(lookup_one_addr(server)) {
            Some(server) => server,
            None => return self,
        };

        self.destination(Destination::Udp { local, server })
    }

    /// Send log entries over UDP to a remote syslog server.
    /// 
    /// **Warning**: Log transmission is not encrypted.
    /// 
    /// This method may block to perform a DNS lookup. If the `server` parameter resolves to more than one socket address, the first one will be used.
    pub fn udp(&mut self, server: impl ToSocketAddrs + Debug) -> &mut Self {
        let server = match self.defer_error(lookup_one_addr(server)) {
            Some(server) => server,
            None => return self,
        };

        self.destination(Destination::Udp { local: None, server })
    }

    /// Sets a custom process ID to include with log messages.
    /// 
    /// By default, the actual process ID of the process is used.
    pub fn pid(&mut self, pid: u32) -> &mut Self {
        self.pid = Some(pid);
        self
    }

    /// Sets the name of this process, for inclusion with log messages.
    /// 
    /// By default, this is inferred from [the name of the executable].
    /// 
    /// [the name of the executable]: https://doc.rust-lang.org/std/env/fn.current_exe.html
    pub fn process_name(&mut self, process_name: impl Into<Cow<'static, str>>) -> &mut Self {
        self.process_name = Some(process_name.into());
        self
    }

    /// Sets the log level of this logger.
    pub fn level(&mut self, severity: Severity) -> &mut Self {
        self.common.level = severity;
        self
    }

    /// Sets the size of the asynchronous channel of this logger.
    pub fn channel_size(&mut self, channel_size: usize) -> &mut Self {
        self.common.channel_size = channel_size;
        self
    }

    /// Sets [`KVFilter`].
    ///
    /// [`KVFilter`]: https://docs.rs/slog-kvfilter/0.6/slog_kvfilter/struct.KVFilter.html
    #[cfg(feature = "slog-kvfilter")]
    pub fn kvfilter(&mut self, parameters: KVFilterParameters) -> &mut Self {
        self.common.kvfilterparameters = Some(parameters);
        self
    }

    /// Sets a custom `MsgFormat3164` implementation.
    /// 
    /// The default is [`BasicMsgFormat3164`].
    /// 
    /// # Example
    /// 
    /// ```
    /// use sloggers::Build;
    /// use sloggers::syslog::SyslogBuilder;
    /// use slog_syslog::NullMsgFormat3164;
    /// 
    /// let logger = SyslogBuilder::new()
    ///     .msg_format_3164(NullMsgFormat3164)
    ///     .build()
    ///     .expect("failed to construct logger");
    /// ```
    /// 
    /// [`BasicMsgFormat3164`]: https://docs.rs/slog-syslog/0.13/slog_syslog/struct.BasicMsgFormat3164.html
    pub fn msg_format_3164(&mut self, format: impl MsgFormat3164 + Clone + Debug + Send + 'static) -> &mut Self {
        self.msg_format_3164 = Box::new(format);
        self
    }
}

impl Build for SyslogBuilder {
    fn build(&self) -> Result<Logger> {
        if let Some(error) = &self.deferred_error {
            return Err(error.clone());
        }

        let msg_format_3164 = clone_box(&*self.msg_format_3164);
        let facility = self.facility;
        let hostname = self.hostname.clone();
        let destination = self.destination.clone();
        let pid = self.pid;
        let process_name = self.process_name.clone();

        let drain = Retry::new(move || {
            // `slog_syslog::SyslogBuilder` consumes `self` with every method call, and this `SyslogBuilder` doesn't, so we'll need a lot of `let b =` here.
            let b = slog_syslog::SyslogBuilder::new()
                .msg_format(clone_box(&*msg_format_3164));

            let b = match facility {
                Some(facility) => b.facility(facility),
                None => b,
            };

            let b = match &hostname {
                Some(hostname) => b.hostname(hostname.as_ref().to_owned()),
                None => b,
            };

            let b = match &destination {
                Destination::Local => b,
                Destination::Tcp { server } => b.tcp(*server),
                Destination::Udp { local, server } => {
                    let local: SocketAddr = match (local, server) {
                        (Some(local), _) => *local,
                        (None, SocketAddr::V4(_)) => (Ipv4Addr::UNSPECIFIED, 0u16).into(),
                        (None, SocketAddr::V6(_)) => (Ipv6Addr::UNSPECIFIED, 0u16).into(),
                    };

                    b.udp(local, *server)
                },
                Destination::Unix { socket } => b.unix(socket.as_ref().to_owned()),
            };

            let b = match pid {
                Some(pid) => b.pid(pid as i32),
                None => b,
            };

            let b = match &process_name {
                Some(process_name) => b.process(process_name.as_ref().to_owned()),
                None => b,
            };

            b.start_single_threaded()
        }).map_err(|error| -> Error {
            // `syslog::Error` is `!Sync` (`error_chain` errors are `Send` but not `Sync`), so it cannot be used as the cause of a `sloggers::Error` (`trackable` requires errors to be `Sync`). FML.
            ErrorKind::ServerConnect.cause(error.to_string()).into()
        })?;

        Ok(self.common.build_with_drain(drain))
    }
}

/// The destination to which log records will be outputted.
///
/// # Examples
///
/// The default value:
///
/// ```
/// use sloggers::syslog::Destination;
///
/// assert_eq!(Destination::default(), Destination::Local);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "type")]
#[non_exhaustive]
pub enum Destination {
    /// Send to local syslog server.
    /// 
    /// On Unix-like platforms, this uses the Unix-domain socket `/dev/log` or `/var/run/log`. On other platforms, this sends UDP packets to 127.0.0.1:514.
    Local,

    /// Send to local syslog server at a specific Unix-domain socket.
    Unix {
        /// The path to the Unix-domain socket to use.
        /// 
        /// This accepts either a `&'static Path` or a `PathBuf`. Call `.into()` on it to place it in this field. For example:
        /// 
        /// ```
        /// use sloggers::syslog::Destination;
        /// use std::path::PathBuf;
        /// 
        /// let unix_path = PathBuf::from("/var/run/log");
        /// 
        /// let destination = Destination::Unix {
        ///     socket: unix_path.into(),
        /// };
        /// ```
        socket: Cow<'static, Path>,
    },

    /// Send to a remote syslog server over TCP.
    /// 
    /// **Warning**: Log transmission is not encrypted.
    Tcp {
        /// Address of the remote server.
        server: SocketAddr,
    },

    /// Send to a remote syslog server over UDP.
    /// 
    /// **Warning**: Log transmission is not encrypted.
    Udp {
        /// Local address to bind to.
        local: Option<SocketAddr>,

        /// Address of the remote server.
        server: SocketAddr,
    },
}

impl Default for Destination {
    fn default() -> Self {
        Destination::Local
    }
}

fn lookup_one_addr(addr: impl ToSocketAddrs + Debug) -> Result<SocketAddr> {
    match addr.to_socket_addrs() {
        Ok(mut i) => match i.next() {
            Some(a) => Ok(a),
            None => Err(ErrorKind::ServerLookup.cause(format!("no addresses found for {:?}", addr)).into()),
        },
        Err(error) => Err(ErrorKind::ServerLookup.cause(error).into()),
    }
}

/// A trait that simply combines `MsgFormat3164`, `Debug`, and `Send`.
/// 
/// This is needed because Rust cannot (yet) make a trait object for more than one trait. See [rust-lang/rfcs#2035](https://github.com/rust-lang/rfcs/issues/2035) for more information.
trait MsgFormat3164CloneDebugSend: MsgFormat3164 + DynClone + Debug + Send {}
impl<F: MsgFormat3164 + DynClone + Debug + Send> MsgFormat3164CloneDebugSend for F {}
