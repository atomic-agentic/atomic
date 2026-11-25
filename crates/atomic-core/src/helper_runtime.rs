// SPDX-License-Identifier: Apache-2.0
//! Captured helper modules execute once in a bounded Boa child.
mod process;
#[cfg(test)]
mod tests;
use crate::js_modules::Modules;
use boa_engine::{
    Context, JsNativeError, JsResult, JsString, JsValue, Module, Source, context::HostHooks,
    realm::Realm,
};
pub use process::{CHILD_ARG, HostCommand, serve, set_host_command};
use serde_json::Value;
use std::{collections::BTreeMap, rc::Rc, time::Duration};

/// The URL scheme captured modules are keyed by.
pub const SCHEME: &str = "atomic-helper";

/// Fingerprint of the engine, dependency versions, helper host and call protocol.
/// Durable runs verify this identity before resuming helper steps.
pub fn identity() -> String {
    env!("ATOMIC_HELPER_HASH").to_owned()
}

/// A captured helper module graph, already transpiled to JavaScript.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Captured {
    /// JavaScript source for each module, keyed by `atomic-helper:///` URL.
    pub modules: BTreeMap<String, String>,
    /// The URL of the module whose exports are called.
    pub entry: String,
}

impl Captured {
    /// The capture's digest, as the compiler computes it: SHA-256 of
    /// `{"entry":…,"modules":{…}}` with the modules in key order.
    pub fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        #[derive(serde::Serialize)]
        struct Canonical<'a> {
            entry: &'a str,
            modules: &'a BTreeMap<String, String>,
        }
        let text = serde_json::to_string(&Canonical {
            entry: &self.entry,
            modules: &self.modules,
        })
        .unwrap_or_default();
        format!("{:x}", Sha256::digest(text.as_bytes()))
    }
}

/// The limits on one helper call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Limits {
    /// Wall-clock time for the call, including module evaluation.
    pub deadline: Duration,
    /// engine heap, in bytes.
    pub heap_bytes: usize,
    /// ArrayBuffer and typed-array contents held at once, in bytes.
    pub array_buffer_bytes: usize,
    /// The JSON result, in UTF-8 bytes.
    pub output_bytes: usize,
    /// The text of an error the helper throws, in UTF-8 bytes.
    pub error_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(1),
            heap_bytes: 128 * 1024 * 1024,
            array_buffer_bytes: 64 * 1024 * 1024,
            output_bytes: 1024 * 1024,
            error_bytes: 16 * 1024,
        }
    }
}

/// The time and randomness one helper call sees. A durable step records its
/// logical time and seed when it first starts, and the host derives each
/// call's clock from that record, so a retried step's helpers see the same
/// values (see `astro::StepClock`).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Clock {
    /// What `Date.now()` and `Temporal.Now` return, in milliseconds since the
    /// epoch.
    pub now_ms: f64,
    /// The state `Math.random` starts from.
    pub seed: [u32; 4],
}

/// Why a helper call produced no result. Every variant is a failure of the
/// helper, never a value it returned.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, serde::Serialize, serde::Deserialize)]
pub enum Failure {
    /// The helper threw, or a module in its graph failed to evaluate.
    #[error("helper threw: {0}")]
    Threw(String),
    /// The call ran past its deadline.
    #[error("helper deadline exceeded")]
    Deadline,
    /// The call exhausted its memory: the engine heap, or the helper
    /// process's limit on what it may allocate outside the heap.
    #[error("helper memory limit exceeded")]
    Heap,
    /// The call asked for more ArrayBuffer memory than its budget, across live backing stores.
    #[error("helper ArrayBuffer limit exceeded")]
    ArrayBufferLimit,
    /// The result was larger than the output limit.
    #[error("helper output limit exceeded")]
    OutputLimit,
    /// The thrown error's text was larger than the error limit.
    #[error("helper error limit exceeded")]
    ErrorLimit,
    /// The result was not JSON.
    #[error("helper did not return JSON: {0}")]
    NotJson(String),
    /// The graph imports a module outside the capture, or imports
    /// dynamically, or the export is missing.
    #[error("helper module refused: {0}")]
    Refused(String),
    /// The host could not run the call.
    #[error("helper host failed: {0}")]
    Host(String),
}

/// The controls this runtime applies, each of which a test switches off once
/// to show that its test fails without it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Controls {
    /// Remove `Deno`, `__bootstrap`, `console`, `WebAssembly`,
    /// `SharedArrayBuffer` and resizable ArrayBuffers.
    pub strip_globals: bool,
    /// Enforce the child’s live allocation ceiling.
    pub heap_callback: bool,
    /// Bound ArrayBuffer memory with the budgeted allocator.
    pub bounded_array_buffers: bool,
    /// Measure the result and error text before copying them out.
    pub text_limits: bool,
    /// Terminate the call at its deadline.
    pub watchdog: bool,
    /// Replace the clock, randomness, timezone and default locale with the
    /// call's [`Clock`], UTC and `en-US`.
    pub virtual_clock: bool,
}

impl Default for Controls {
    fn default() -> Self {
        Self {
            strip_globals: true,
            heap_callback: true,
            bounded_array_buffers: true,
            text_limits: true,
            watchdog: true,
            virtual_clock: true,
        }
    }
}

// Runs before any author module. The per-call wrapper uses only the functions
// captured here, which author code cannot replace.
const HOST: &str = r#"(() => {
  const freeze = Object.freeze, ownKeys = Reflect.ownKeys, parse = JSON.parse, stringify = JSON.stringify;
  const finite = Number.isFinite, toText = String, HostError = Error;
  function deepFreeze(value) {
    if (value !== null && typeof value === 'object') {
      const keys = ownKeys(value);
      for (let i = 0; i < keys.length; i++) deepFreeze(value[keys[i]]);
      freeze(value);
    }
    return value;
  }
  function jsonOnly(_, value) {
    if (typeof value === 'function' || typeof value === 'symbol' || typeof value === 'bigint'
      || (typeof value === 'number' && !finite(value))) throw new HostError('non-JSON helper output');
    return value;
  }
  Object.defineProperty(globalThis, '__atomicHost', { configurable: false, writable: false, enumerable: false,
    value: freeze({ input: text => deepFreeze(parse(text)), output: value => stringify(value, jsonOnly),
      text: value => { const message = toText(value); const stack = value !== null && typeof value === 'object' ? value.stack : undefined; return typeof stack === 'string' ? message + '\n' + stack : message; } }) });
})();
"#;

// Helpers get a quiet console and fixed-size ArrayBuffers. The original
// constructor must also be removed from the prototype chain.
const STRIP: &str = r#"(() => {
  delete globalThis.Deno;
  delete globalThis.__bootstrap;
  delete globalThis.WebAssembly;
  delete globalThis.SharedArrayBuffer;
  delete globalThis.TypedArray;
  const freeze = Object.freeze, construct = Reflect.construct, HostRangeError = RangeError;
  const quiet = () => {};
  Object.defineProperty(globalThis, 'console', { configurable: false, writable: false, enumerable: false,
    value: freeze({ log: quiet, info: quiet, warn: quiet, error: quiet, debug: quiet, trace: quiet }) });
  const Original = globalThis.ArrayBuffer, prototype = Original.prototype;
  const Guarded = function ArrayBuffer(length) {
    if (new.target === undefined) throw new TypeError("Constructor ArrayBuffer requires 'new'");
    if (arguments.length > 1 && arguments[1] !== undefined) throw new HostRangeError('resizable ArrayBuffers are not available to helpers');
    return construct(Original, [length], new.target === Guarded ? Original : new.target);
  };
  Guarded.prototype = prototype;
  Guarded.isView = Original.isView;
  Object.defineProperty(prototype, 'constructor', { value: Guarded, writable: true, configurable: true, enumerable: false });
  Object.defineProperty(globalThis, 'ArrayBuffer', { value: Guarded, writable: true, configurable: true, enumerable: false });
  delete prototype.resize;
})();
"#;

// What a helper could otherwise read from the host: the clock (Date,
// Temporal.Now), randomness (Math.random), and the host's timezone and
// locale. Each call supplies its logical time and four-word random seed.
const CLOCK: &str = r#"((now, a, b, c, d) => {
  const defineProperty = Object.defineProperty, construct = Reflect.construct, freeze = Object.freeze;
  const define = (target, name, value) => defineProperty(target, name, { value, writable: true, configurable: true, enumerable: false });
  const LOCALE = 'en-US';
  const supported = Intl.DateTimeFormat.supportedLocalesOf.bind(Intl.DateTimeFormat);
  const locales = value => value === undefined || supported(value).length === 0 ? LOCALE : value;
  const utc = (options) => (options !== undefined && options !== null && typeof options === 'object' && options.timeZone !== undefined)
    ? options : Object.assign({}, options, { timeZone: 'UTC' });

  // Math.random: sfc32 from the call's seed.
  let s0 = a >>> 0, s1 = b >>> 0, s2 = c >>> 0, s3 = d >>> 0;
  const next = () => {
    const t = (((s0 + s1) | 0) + s3) | 0;
    s3 = (s3 + 1) | 0; s0 = s1 ^ (s1 >>> 9); s1 = (s2 + (s2 << 3)) | 0;
    s2 = (s2 << 21) | (s2 >>> 11); s2 = (s2 + t) | 0;
    return (t >>> 0) / 4294967296;
  };
  for (let i = 0; i < 15; i++) next();
  define(Math, 'random', function random() { return next(); });

  // Date: the logical time, and local time is UTC.
  const OriginalDate = Date, proto = OriginalDate.prototype, UTC = OriginalDate.UTC, parse = OriginalDate.parse;
  const isoLocal = /^[+-]?\d{4,6}-\d{2}-\d{2}T\d{2}:\d{2}(?::\d{2}(?:\.\d{1,9})?)?$/;
  const asUTC = (value) => (typeof value === 'string' && isoLocal.test(value) ? value + 'Z' : value);
  const VirtualDate = function Date(...args) {
    if (new.target === undefined) return new VirtualDate().toString();
    const fields = args.length === 0 ? [now] : args.length === 1 ? [asUTC(args[0])] : [UTC(...args)];
    return construct(OriginalDate, fields, new.target === VirtualDate ? OriginalDate : new.target);
  };
  VirtualDate.prototype = proto;
  define(VirtualDate, 'now', function now_() { return now; });
  define(VirtualDate, 'parse', function parse_(text) { return parse(asUTC(text)); });
  define(VirtualDate, 'UTC', UTC);
  define(proto, 'constructor', VirtualDate);
  define(globalThis, 'Date', VirtualDate);
  for (const name of ['FullYear', 'Month', 'Date', 'Day', 'Hours', 'Minutes', 'Seconds', 'Milliseconds']) {
    define(proto, 'get' + name, proto['getUTC' + name]);
    if (name !== 'Day') define(proto, 'set' + name, proto['setUTC' + name]);
  }
  define(proto, 'getTimezoneOffset', function getTimezoneOffset() { return 0; });
  const days = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
  const months = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
  const pad = (n, w = 2) => String(n).padStart(w, '0');
  const valid = (date) => !Number.isNaN(proto.getTime.call(date));
  const dateText = (date) => `${days[proto.getUTCDay.call(date)]} ${months[proto.getUTCMonth.call(date)]} ${pad(proto.getUTCDate.call(date))} ${pad(proto.getUTCFullYear.call(date), 4)}`;
  const timeText = (date) => `${pad(proto.getUTCHours.call(date))}:${pad(proto.getUTCMinutes.call(date))}:${pad(proto.getUTCSeconds.call(date))} GMT+0000 (Coordinated Universal Time)`;
  define(proto, 'toString', function toString() { return valid(this) ? `${dateText(this)} ${timeText(this)}` : 'Invalid Date'; });
  define(proto, 'toDateString', function toDateString() { return valid(this) ? dateText(this) : 'Invalid Date'; });
  define(proto, 'toTimeString', function toTimeString() { return valid(this) ? timeText(this) : 'Invalid Date'; });
  for (const name of ['toLocaleString', 'toLocaleDateString', 'toLocaleTimeString']) {
    const original = proto[name];
    define(proto, name, { [name](l, options) { return original.call(this, locales(l), utc(options)); } }[name]);
  }

  // Intl: an unspecified locale is en-US, and dates format in UTC.
  for (const name of ['Collator', 'DateTimeFormat', 'DisplayNames', 'DurationFormat', 'ListFormat', 'NumberFormat', 'PluralRules', 'RelativeTimeFormat', 'Segmenter']) {
    const Original = Intl[name];
    if (typeof Original !== 'function') continue;
    const dates = name === 'DateTimeFormat';
    const Wrapped = { [name]: function (l, options) {
      return construct(Original, [locales(l), dates ? utc(options) : options], new.target === undefined || new.target === Wrapped ? Original : new.target);
    } }[name];
    Wrapped.prototype = Original.prototype;
    define(Wrapped, 'supportedLocalesOf', Original.supportedLocalesOf);
    define(Original.prototype, 'constructor', Wrapped);
    define(Intl, name, Wrapped);
  }
  // A DateTimeFormat given no date formats the current time, which engines read
  // from the host clock: the logical time is passed instead.
  const DateTimeProto = Intl.DateTimeFormat.prototype;
  const formatGetter = Object.getOwnPropertyDescriptor(DateTimeProto, 'format').get, bound = new WeakMap();
  defineProperty(DateTimeProto, 'format', { configurable: true, enumerable: false, get() {
    const original = formatGetter.call(this);
    let format = bound.get(this);
    if (format === undefined) {
      format = (date) => original(date === undefined ? now : date);
      bound.set(this, format);
    }
    return format;
  } });
  const formatToParts = DateTimeProto.formatToParts;
  if (formatToParts) define(DateTimeProto, 'formatToParts', function formatToParts_(date) { return formatToParts.call(this, date === undefined ? now : date); });
  for (const [target, name, args] of [[Number.prototype, 'toLocaleString', 0], [BigInt.prototype, 'toLocaleString', 0],
    [String.prototype, 'localeCompare', 1], [String.prototype, 'toLocaleLowerCase', 0], [String.prototype, 'toLocaleUpperCase', 0]]) {
    const original = target[name];
    define(target, name, { [name](...given) { given[args] = locales(given[args]); return original.apply(this, given); } }[name]);
  }

  // Temporal: Now is the logical time in UTC; formatting follows Intl above.
  if (typeof Temporal === 'object' && Temporal !== null) {
    const T = Temporal;
    const instant = () => T.Instant.fromEpochMilliseconds(now);
    const zoned = (zone = 'UTC') => instant().toZonedDateTimeISO(zone);
    define(T, 'Now', freeze({
      instant, timeZoneId: () => 'UTC', zonedDateTimeISO: zoned,
      plainDateTimeISO: (zone) => zoned(zone).toPlainDateTime(),
      plainDateISO: (zone) => zoned(zone).toPlainDate(),
      plainTimeISO: (zone) => zoned(zone).toPlainTime(),
      [Symbol.toStringTag]: 'Temporal.Now',
    }));
    for (const type of ['Instant', 'PlainDate', 'PlainDateTime', 'PlainMonthDay', 'PlainTime', 'PlainYearMonth', 'Duration']) {
      const target = T[type]?.prototype;
      if (!target || typeof target.toLocaleString !== 'function') continue;
      const original = target.toLocaleString, zone = type === 'Instant';
      define(target, 'toLocaleString', function toLocaleString(l, options) { return original.call(this, locales(l), zone ? utc(options) : options); });
    }
    const ZonedProto = T.ZonedDateTime?.prototype;
    if (ZonedProto) {
      const original = ZonedProto.toLocaleString;
      define(ZonedProto, 'toLocaleString', function toLocaleString(l, options) { return original.call(this, locales(l), options); });
    }
  }
})"#;
struct Hooks {
    restricted: bool,
}
impl HostHooks for Hooks {
    fn ensure_can_compile_strings(
        &self,
        _: Realm,
        _: &[JsString],
        _: &JsString,
        _: bool,
        _: &mut Context,
    ) -> JsResult<()> {
        if self.restricted {
            Err(JsNativeError::eval()
                .with_message("string compilation is disabled")
                .into())
        } else {
            Ok(())
        }
    }
    fn local_timezone_offset_seconds(&self, _: i64) -> i32 {
        0
    }
}

struct LogicalTime(i64);
impl boa_engine::context::time::Clock for LogicalTime {
    fn now(&self) -> boa_engine::context::time::JsInstant {
        boa_engine::context::time::JsInstant::new(0, 0)
    }
    fn system_time_millis(&self) -> i64 {
        self.0
    }
}

pub(crate) struct Isolate {
    context: Context,
    modules: Rc<Modules>,
    entry: String,
    limits: Limits,
    controls: Controls,
}
impl Isolate {
    pub(crate) fn new(
        captured: &Captured,
        limits: Limits,
        clock: Clock,
        controls: Controls,
    ) -> Result<Self, Failure> {
        let modules = Rc::new(Modules::default());
        let mut builder = Context::builder()
            .module_loader(modules.clone())
            .host_hooks(Rc::new(Hooks {
                restricted: controls.strip_globals,
            }));
        if controls.virtual_clock {
            if !clock.now_ms.is_finite() {
                return Err(Failure::Host("helper clock is not a finite time".into()));
            }
            builder = builder.clock(Rc::new(LogicalTime(clock.now_ms as i64)));
        }
        let mut context = builder.build().map_err(|e| Failure::Host(e.to_string()))?;
        let mut init = |src: &str| {
            context
                .eval(Source::from_bytes(src))
                .map_err(|e| Failure::Host(e.to_string()))
        };
        init(HOST)?;
        if controls.strip_globals {
            init(STRIP)?;
        }
        if controls.virtual_clock {
            let [a, b, c, d] = clock.seed;
            init(&format!("{CLOCK}({}, {a}, {b}, {c}, {d})", clock.now_ms))?;
        }
        for (name, source) in &captured.modules {
            if !name.starts_with("atomic-helper:///") {
                return Err(Failure::Refused("invalid helper module URL".into()));
            }
            let module = Module::parse(
                Source::from_bytes(source).with_path(std::path::Path::new(name)),
                None,
                &mut context,
            )
            .map_err(|e| Failure::Threw(e.to_string()))?;
            modules.insert(name.clone(), module);
        }
        Ok(Self {
            context,
            modules,
            entry: captured.entry.clone(),
            limits,
            controls,
        })
    }

    pub(crate) fn call(&mut self, export: &str, args: &Value) -> Result<Value, Failure> {
        if !args.is_array() {
            return Err(Failure::Host("helper arguments must be an array".into()));
        }
        let quote = |s: &str| serde_json::Value::from(s).to_string();
        let wrapper = format!(
            r#"import * as helpers from {};
            export const missing = typeof helpers[{}] !== 'function';
            export let result, error;
            if (!missing) {{
                try {{ result = __atomicHost.output(await helpers[{}](...__atomicHost.input({}))); }}
                catch (e) {{ error = __atomicHost.text(e); }}
            }}"#,
            quote(&self.entry),
            quote(export),
            quote(export),
            quote(&args.to_string())
        );
        let ctx = &mut self.context;
        let module = Module::parse(Source::from_bytes(&wrapper), None, ctx)
            .map_err(|e| Failure::Host(e.to_string()))?;
        if let Err(e) = self.modules.evaluate(&module, ctx) {
            let reason = e
                .into_opaque(ctx)
                .and_then(|v| v.to_string(ctx))
                .map_err(|_| Failure::Threw("module evaluation failed".into()))?;
            let mut message = String::new();
            for c in reason.as_str().code_points_lossy() {
                if message.len() + c.len_utf8() > self.limits.error_bytes {
                    message.push('…');
                    break;
                }
                message.push(c);
            }
            return Err(if message.contains("uncaptured module") {
                Failure::Refused(message)
            } else {
                Failure::Threw(message)
            });
        }
        let mut get = |key: &str| {
            module
                .get_value(JsString::from(key), ctx)
                .map_err(|e| Failure::Host(e.to_string()))
        };
        if get("missing")?.as_boolean() == Some(true) {
            return Err(Failure::Refused(format!(
                "{export} is not an exported function"
            )));
        }
        let checked = |val: JsValue, limit, failure| -> Result<Option<String>, Failure> {
            let Some(s) = val.as_string() else {
                return Ok(None);
            };
            if self.controls.text_limits
                && s.as_str()
                    .code_points_lossy()
                    .map(char::len_utf8)
                    .sum::<usize>()
                    > limit
            {
                return Err(failure);
            }
            Ok(Some(s.to_std_string_lossy()))
        };
        if let Some(error) = checked(get("error")?, self.limits.error_bytes, Failure::ErrorLimit)? {
            return Err(Failure::Threw(error));
        }
        let output = checked(
            get("result")?,
            self.limits.output_bytes,
            Failure::OutputLimit,
        )?
        .ok_or_else(|| Failure::NotJson("the helper returned undefined".into()))?;
        serde_json::from_str(&output).map_err(|e| Failure::NotJson(e.to_string()))
    }
}

/// Invoke a captured export in a fresh supervised child.
pub fn invoke(
    captured: &Captured,
    export: &str,
    args: &Value,
    limits: Limits,
    clock: Clock,
) -> Result<Value, Failure> {
    process::invoke(captured, export, args, limits, clock)
}

pub(crate) fn invoke_with(
    captured: &Captured,
    export: &str,
    args: &Value,
    limits: Limits,
    clock: Clock,
    controls: Controls,
) -> Result<Value, Failure> {
    Isolate::new(captured, limits, clock, controls)?.call(export, args)
}
