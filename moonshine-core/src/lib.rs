// TODO: Remove this when a proper error type is implemented for all functions that return `Result<(), ()>`.
#![allow(clippy::result_unit_err)]

pub mod app_scanner;
pub mod clients;
pub mod config;
pub(crate) mod crypto;
pub mod discovery;
pub mod rtsp;
pub mod session;
pub(crate) mod state;
pub mod tls;
pub mod webserver;

/// Reasons for initiating a global shutdown.
///
/// Used as the type parameter for `ShutdownManager<ShutdownReason>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownReason {
	/// Application quit signal (Ctrl+C or SIGTERM).
	AppQuit = 1,
	/// HTTP webserver is shutting down.
	HttpShutdown = 2,
	/// HTTPS webserver is shutting down.
	HttpsShutdown = 3,
	/// RTSP server is shutting down.
	RtspShutdown = 4,
	/// Session manager guard token (trigger_shutdown_token, not a shutdown trigger).
	SessionManagerShutdown = 5,
}
