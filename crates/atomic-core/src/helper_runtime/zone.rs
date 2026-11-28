// SPDX-License-Identifier: Apache-2.0

//! ICU's process-wide defaults, set to UTC and `en-US` for helpers.
//!
//! V8 takes local time and the default locale from ICU, which reads them
//! from the process environment. The prelude replaces the Date and Intl
//! entry points it can reach, but some built-ins (the legacy `Date.parse`
//! formats, `getYear`) read ICU's local time directly. Setting ICU's
//! defaults closes them all. A helper call's process runs nothing but that
//! call, so the process-wide setting reaches nothing else.

use deno_core::v8;
use std::sync::OnceLock;

/// The time zone and locale every helper isolate sees.
pub(super) const ZONE: &str = "UTC";
pub(super) const LOCALE: &str = "en_US";

/// Sets ICU's default time zone and locale, once per process. Called after
/// V8 has loaded ICU's data, so the zone resolves to its canonical id.
pub(super) fn set_defaults() -> Result<(), String> {
    static DONE: OnceLock<Result<(), String>> = OnceLock::new();
    DONE.get_or_init(|| {
        // rusty_v8 152 binds ICU's default zone itself (upstream
        // denoland/rusty_v8#2000), so this no longer names ICU's versioned C
        // symbol. Read the zone back: a refused or ignored id fails closed.
        if !v8::icu::set_default_time_zone(ZONE) {
            return Err(format!("ICU refused {ZONE} as the default time zone"));
        }
        let effective = v8::icu::get_default_time_zone();
        if effective != ZONE {
            return Err(format!(
                "ICU's default time zone is {effective}, not {ZONE}"
            ));
        }
        v8::icu::set_default_locale(LOCALE);
        Ok(())
    })
    .clone()
}
