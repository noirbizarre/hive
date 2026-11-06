//! Coverage isolation for disposable mock-agent subprocesses, never their parent tests.

use std::collections::BTreeMap;

// Brokers may kill fixtures during LLVM's exit-time flush, leaving truncated merge inputs.
pub const PROFILE_FILE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

pub fn env() -> BTreeMap<String, String> {
    BTreeMap::from([("LLVM_PROFILE_FILE".into(), PROFILE_FILE.into())])
}
