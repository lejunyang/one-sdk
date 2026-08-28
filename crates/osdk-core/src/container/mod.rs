//! Native container-runtime contracts.
//!
//! This first foundation deliberately contains no OCI store and performs no
//! native configuration writes. It provides stable diagnostic, redaction, and
//! injectable process boundaries for later read-only runtime adapters.

pub mod redact;
pub mod report;
pub mod runtime;

pub use redact::{
    CommandPurpose, HeaderName, NativeProgram, RedactedCommand, RedactedHeader, RedactedUrl,
    RedactedUrlError, RedactedValue, REDACTED,
};
pub use report::{
    Capability, CapabilityStatus, DiagnosticEvidence, DiagnosticReport, DiagnosticStatus, Endpoint,
    EndpointScope, EndpointTransport, Privilege, RuntimeKind, DIAGNOSTIC_SCHEMA_VERSION,
};
pub use runtime::{ForegroundCommand, ProbeCommand, RuntimeAdapter};
