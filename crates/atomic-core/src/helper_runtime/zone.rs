// SPDX-License-Identifier: Apache-2.0

//! ICU's process-wide defaults, set to UTC and `en-US` for helpers.
//!
//! V8 takes local time and the default locale from ICU, which reads them
//! from the process environment. The prelude replaces the Date and Intl
//! entry points it can reach, but some built-ins (the legacy `Date.parse`
//! formats, `getYear`) read ICU's local time directly. Setting ICU's
//! defaults closes them all. Only helper isolates use V8 in atomic, so the
//! process-wide setting reaches nothing else.

#![allow(unsafe_code)]

use deno_core::v8;
use std::sync::OnceLock;

unsafe extern "C" {
    // ICU's C API, linked into V8's archive. The suffix is ICU's major
    // version, as in `v8::icu::set_common_data_77`; a V8 upgrade that moves
    // ICU fails to link here rather than silently keeping the host zone.
    #[link_name = "ucal_setDefaultTimeZone_77"]
    fn ucal_set_default_time_zone(zone: *const u16, status: *mut i32);
}

/// The time zone and locale every helper isolate sees.
pub(super) const ZONE: &str = "UTC";
pub(super) const LOCALE: &str = "en_US";

/// Sets ICU's default time zone and locale, once per process. Called after
/// V8 has loaded ICU's data, so the zone resolves to its canonical id.
pub(super) fn set_defaults() -> Result<(), String> {
    static DONE: OnceLock<Result<(), String>> = OnceLock::new();
    DONE.get_or_init(|| {
        let zone: Vec<u16> = ZONE.encode_utf16().chain([0]).collect();
        let mut status = 0i32;
        // SAFETY: `zone` is a NUL-terminated UTF-16 string that outlives the
        // call, and `status` a valid out-pointer. ICU copies the id and
        // guards its default zone with its own lock.
        unsafe { ucal_set_default_time_zone(zone.as_ptr(), &mut status) };
        // ICU reports failures as positive codes, warnings as negative ones.
        if status > 0 {
            return Err(format!(
                "ICU refused {ZONE} as the default time zone (error {status})"
            ));
        }
        v8::icu::set_default_locale(LOCALE);
        Ok(())
    })
    .clone()
}
