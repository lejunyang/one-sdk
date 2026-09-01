//! Android SDK support: repository protocol and license acceptance.
//!
//! Google distributes the Android SDK through a versioned XML manifest rather
//! than a package registry. [`repo`] speaks that protocol; [`license`] handles
//! the agreement gate that guards most packages.
//!
//! ## Scope
//!
//! osdk resolves and downloads packages **directly from Google's servers on the
//! user's machine**, and never stores, proxies, or re-serves the archives. That
//! keeps it in the same position as Android Studio's built-in SDK Manager and
//! outside the redistribution terms of the SDK agreement. Introducing a shared
//! binary cache or an internal mirror of the archives would change that and
//! must not be added here.

pub mod license;
pub mod repo;
