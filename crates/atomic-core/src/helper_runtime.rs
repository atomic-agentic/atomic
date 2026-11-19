// SPDX-License-Identifier: Apache-2.0
//! The runtime captured TypeScript helpers run in: one fresh V8 isolate per
//! call, embedded through `deno_core` with none of Deno's extensions.
//!
//! An isolate has no filesystem, network, environment, process, timer or FFI
//! operations, because nothing adds them, so there is nothing to grant or
//! deny: a helper sees ECMAScript built-ins and the JSON arguments it is
//! given, and returns JSON. The prelude removes what `deno_core` installs
//! (`Deno.core`, `__bootstrap` and a printing `console`) and the built-ins
//! whose memory V8 allocates outside both limits below (`WebAssembly`,
//! `SharedArrayBuffer` and resizable `ArrayBuffer`s).
//!
//! Resource limits are enforced from Rust, so a helper cannot lift them: a
//! watchdog thread terminates a call at its deadline, a near-heap-limit
//! callback terminates it before V8's heap is exhausted, a budgeted allocator
//! bounds ArrayBuffer memory, and the result and the text of an error a call
//! throws are measured before they are copied out of the isolate. An error a
//! module throws while it loads is copied by `deno_core` and then truncated,
//! within the memory limit of the call's process. A failed isolate is
//! discarded.
//!
//! [`invoke`] runs each call in a child process (see `process`), which bounds
//! the memory V8 allocates outside its heap and contains the failures V8
//! cannot recover from, such as an invalid string size, by aborting.

mod allocator;
mod process;
#[cfg(test)]
mod tests;
mod zone;

pub use process::{CHILD_ARG, HostCommand, serve, set_host_command};

use deno_core::{
    JsRuntime, ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse, ModuleLoader,
    ModuleSource, ModuleSourceCode, ModuleSpecifier, ModuleType, PollEventLoopOptions,
    ResolutionKind, RuntimeOptions, error::ModuleLoaderError, v8,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The URL scheme captured modules are keyed by.
pub const SCHEME: &str = "atomic-helper";

/// The version of the helper-call semantics: the wrapper, JSON marshalling
/// and failure kinds. Changing what a helper observes changes this.
const SEMANTICS: &str = "atomic-helper-call-2";

/// What a helper's behaviour depends on besides its own code: the engine
/// (deno_core and V8), the prelude (host functions, stripped globals, the
/// virtual clock) and the call semantics. A run that recorded another
/// identity cannot resume its helper steps on this runtime.
pub fn identity() -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for part in [
        SEMANTICS,
        "deno_core 0.413.0",
        deno_core::v8::VERSION_STRING,
        HOST,
        STRIP,
        CLOCK,
        zone::ZONE,
        zone::LOCALE,
    ] {
        digest.update(part.as_bytes());
        digest.update([0]);
    }
    format!("{:x}", digest.finalize())
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
    /// V8 heap, in bytes.
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
    /// The call exhausted its memory: the V8 heap, or the helper
    /// process's limit on what it may allocate outside the heap.
    #[error("helper memory limit exceeded")]
    Heap,
    /// The call asked for more ArrayBuffer memory than its budget, and V8
    /// could not recover the space.
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
    /// Terminate the call from the near-heap-limit callback.
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

struct Loader(BTreeMap<String, String>);

fn invoke_url(url: &ModuleSpecifier) -> bool {
    url.scheme() == SCHEME && url.path().starts_with("/__invoke")
}

impl ModuleLoader for Loader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, ModuleLoaderError> {
        if matches!(kind, ResolutionKind::DynamicImport) {
            return Err(ModuleLoaderError::generic("dynamic imports are disabled"));
        }
        let url =
            deno_core::resolve_import(specifier, referrer).map_err(ModuleLoaderError::from_err)?;
        if !(self.0.contains_key(url.as_str()) || invoke_url(&url)) {
            return Err(ModuleLoaderError::generic(format!(
                "uncaptured module: {url}"
            )));
        }
        Ok(url)
    }

    fn load(
        &self,
        url: &ModuleSpecifier,
        _: Option<&ModuleLoadReferrer>,
        options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        if options.is_dynamic_import {
            return ModuleLoadResponse::Sync(Err(ModuleLoaderError::generic(
                "dynamic imports are disabled",
            )));
        }
        ModuleLoadResponse::Sync(
            self.0
                .get(url.as_str())
                .map(|source| {
                    ModuleSource::new(
                        ModuleType::JavaScript,
                        ModuleSourceCode::String(source.clone().into()),
                        url,
                        None,
                    )
                })
                .ok_or_else(|| ModuleLoaderError::generic(format!("uncaptured module: {url}"))),
        )
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
      text: value => (value !== null && typeof value === 'object' && typeof value.stack === 'string') ? value.stack : toText(value) }) });
})();
"#;

// deno_core installs Deno.core and __bootstrap, which reach its built-in
// operations, and a console that prints to the process's stdout; author code
// gets none of them, and console calls are accepted and discarded.
// WebAssembly memories, SharedArrayBuffers and resizable ArrayBuffers are
// reserved by V8's page allocator, outside both the heap limit and the
// budgeted ArrayBuffer allocator, so they are removed: ArrayBuffer accepts a
// length and nothing else, and the original constructor is unreachable from
// its prototype.
const STRIP: &str = r#"(() => {
  delete globalThis.Deno;
  delete globalThis.__bootstrap;
  delete globalThis.WebAssembly;
  delete globalThis.SharedArrayBuffer;
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
// locale, which ICU reads per process and V8 cannot set per isolate. Each is
// replaced: time is the call's logical time, Math.random a seeded generator,
// local time is UTC, and an unspecified locale is en-US. ICU's own defaults
// are also set to UTC and en-US (see `zone`), for the built-ins this cannot
// reach. Called with the call's time and the four words of its seed.
const CLOCK: &str = r#"((now, a, b, c, d) => {
  const defineProperty = Object.defineProperty, construct = Reflect.construct, freeze = Object.freeze;
  const define = (target, name, value) => defineProperty(target, name, { value, writable: true, configurable: true, enumerable: false });
  const LOCALE = 'en-US';
  const locales = (value) => (value === undefined ? LOCALE : value);
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
  // A DateTimeFormat given no date formats the current time, which V8 reads
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
  define(DateTimeProto, 'formatToParts', function formatToParts_(date) { return formatToParts.call(this, date === undefined ? now : date); });
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

/// Flags the watchdog and heap callback raise before they terminate a call,
/// so the failure is reported as the limit it hit.
#[derive(Default)]
struct Tripped {
    deadline: AtomicBool,
    heap: AtomicBool,
}

/// One V8 isolate holding a captured module graph. Module state persists
/// between calls on one isolate, so [`invoke`] uses a fresh one for every
/// call; reusing one is only for measurement and negative controls.
pub(crate) struct Isolate {
    // Declared first, so it drops before the Tokio runtime it was created in.
    runtime: JsRuntime,
    tokio: tokio::runtime::Runtime,
    entry: String,
    limits: Limits,
    controls: Controls,
    tripped: Arc<Tripped>,
    budget: Option<Arc<allocator::Budget>>,
    calls: u64,
}

impl Isolate {
    /// Creates an isolate for `captured` with the prelude applied.
    pub(crate) fn new(
        captured: &Captured,
        limits: Limits,
        clock: Clock,
        controls: Controls,
    ) -> Result<Self, Failure> {
        let host = |error: &dyn std::fmt::Display| Failure::Host(error.to_string());
        // V8 posts delayed tasks to the Tokio runtime current when the
        // isolate is created; creating one outside a runtime aborts.
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(|error| host(&error))?;
        let mut params = v8::CreateParams::default().heap_limits(0, limits.heap_bytes);
        let mut budget = None;
        if controls.bounded_array_buffers {
            let (allocator, shared) = allocator::bounded(limits.array_buffer_bytes);
            params = params.array_buffer_allocator(allocator);
            budget = Some(shared);
        }
        if controls.virtual_clock {
            // ICU's defaults are set once V8 has loaded ICU's data and before
            // any isolate exists, so no isolate caches the host's.
            JsRuntime::init_platform(None);
            zone::set_defaults().map_err(Failure::Host)?;
        }
        let mut runtime = {
            let _entered = tokio.enter();
            JsRuntime::new(RuntimeOptions {
                module_loader: Some(Rc::new(Loader(captured.modules.clone()))),
                create_params: Some(params),
                ..Default::default()
            })
        };
        if controls.strip_globals {
            // `eval` and `new Function` compile strings; a helper is its
            // captured modules and nothing else.
            let context = runtime.main_context();
            deno_core::scope!(scope, &mut runtime);
            v8::Local::new(scope, context).set_allow_generation_from_strings(false);
        }
        let tripped = Arc::new(Tripped::default());
        if controls.heap_callback {
            let handle = runtime.v8_isolate().thread_safe_handle();
            let flag = tripped.clone();
            // Terminating unwinds the call; the raised limit gives V8 room to
            // do that instead of aborting. V8 also calls this from the
            // last-resort collection after the allocator refuses an
            // ArrayBuffer, which `call` reports as that limit. Each call
            // raises the limit by a quarter of the original, to twice it at
            // most; past that V8 aborts, which the call's process contains.
            runtime.add_near_heap_limit_callback(move |current, initial| {
                flag.heap.store(true, Ordering::Release);
                handle.terminate_execution();
                current
                    .saturating_add(initial / 4)
                    .min(initial.saturating_mul(2))
                    .max(current)
            });
        }
        runtime
            .execute_script("atomic:host", HOST)
            .map_err(|error| host(&error))?;
        if controls.strip_globals {
            runtime
                .execute_script("atomic:strip", STRIP)
                .map_err(|error| host(&error))?;
        }
        if controls.virtual_clock {
            if !clock.now_ms.is_finite() {
                return Err(Failure::Host("helper clock is not a finite time".into()));
            }
            let [a, b, c, d] = clock.seed;
            runtime
                .execute_script(
                    "atomic:clock",
                    format!("{CLOCK}({:?}, {a}, {b}, {c}, {d});", clock.now_ms),
                )
                .map_err(|error| host(&error))?;
        }
        Ok(Self {
            runtime,
            tokio,
            entry: captured.entry.clone(),
            limits,
            controls,
            tripped,
            budget,
            calls: 0,
        })
    }

    /// Calls one export with `args` (a JSON array) spread as its parameters.
    pub(crate) fn call(&mut self, export: &str, args: &Value) -> Result<Value, Failure> {
        let _entered = self.tokio.enter();
        let handle = self.runtime.v8_isolate().thread_safe_handle();
        let (done, finished) = std::sync::mpsc::channel::<()>();
        let deadline = self.limits.deadline;
        let watchdog = self.controls.watchdog.then(|| {
            let flag = self.tripped.clone();
            std::thread::spawn(move || {
                if finished.recv_timeout(deadline).is_err() {
                    flag.deadline.store(true, Ordering::Release);
                    handle.terminate_execution();
                }
            })
        });
        self.calls += 1;
        let (runtime, entry, sequence) = (&mut self.runtime, self.entry.as_str(), self.calls);
        let (limits, controls) = (self.limits, self.controls);
        let result = self.tokio.block_on(async {
            match tokio::time::timeout(
                deadline + Duration::from_millis(250),
                call(runtime, entry, sequence, export, args, limits, controls),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(Failure::Deadline),
            }
        });
        let _ = done.send(());
        if let Some(watchdog) = watchdog {
            watchdog
                .join()
                .map_err(|_| Failure::Host("watchdog panicked".into()))?;
        }
        if self.tripped.heap.load(Ordering::Acquire) {
            let refused = self.budget.as_ref().is_some_and(|budget| budget.refused());
            return Err(if refused {
                Failure::ArrayBufferLimit
            } else {
                Failure::Heap
            });
        }
        if self.tripped.deadline.load(Ordering::Acquire) {
            return Err(Failure::Deadline);
        }
        result
    }
}

fn bounded(text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

// Reads a global the wrapper set, measuring it before copying it out.
fn read_text(
    runtime: &mut JsRuntime,
    name: &'static str,
    limit: usize,
    measured: bool,
) -> Result<Option<String>, Failure> {
    let value = runtime
        .execute_script("atomic:result", name)
        .map_err(|error| Failure::Host(error.to_string()))?;
    deno_core::scope!(scope, runtime);
    let value = v8::Local::new(scope, value);
    if value.is_undefined() {
        return Ok(None);
    }
    let text = v8::Local::<v8::String>::try_from(value)
        .map_err(|_| Failure::Host(format!("{name} is not text")))?;
    if measured && text.utf8_length(scope) > limit {
        return Err(Failure::OutputLimit);
    }
    Ok(Some(text.to_rust_string_lossy(scope)))
}

async fn call(
    runtime: &mut JsRuntime,
    entry: &str,
    sequence: u64,
    export: &str,
    args: &Value,
    limits: Limits,
    controls: Controls,
) -> Result<Value, Failure> {
    if !args.is_array() {
        return Err(Failure::Host("helper arguments must be an array".into()));
    }
    let quote = |text: &str| serde_json::to_string(text).unwrap_or_default();
    let code = format!(
        r#"
        import * as authored from {entry};
        const host = globalThis.__atomicHost;
        globalThis.__atomicResult = undefined;
        globalThis.__atomicError = undefined;
        globalThis.__atomicMissing = typeof authored[{export}] !== 'function';
        if (!globalThis.__atomicMissing) {{
          try {{
            globalThis.__atomicResult = host.output(await authored[{export}](...host.input({args})));
          }} catch (error) {{ globalThis.__atomicError = host.text(error); }}
        }}
    "#,
        entry = quote(entry),
        export = quote(export),
        args = quote(&args.to_string()),
    );
    let specifier = ModuleSpecifier::parse(&format!("{SCHEME}:///__invoke_{sequence}.mjs"))
        .map_err(|error| Failure::Host(error.to_string()))?;
    let threw = |error: deno_core::error::CoreError| {
        let text = error.to_string();
        if text.contains("uncaptured module") || text.contains("dynamic imports are disabled") {
            Failure::Refused(bounded(text, limits.error_bytes))
        } else {
            Failure::Threw(bounded(text, limits.error_bytes))
        }
    };
    let id = runtime
        .load_side_es_module_from_code(&specifier, code)
        .await
        .map_err(threw)?;
    let evaluated = runtime.mod_evaluate(id);
    runtime
        .run_event_loop(PollEventLoopOptions::default())
        .await
        .map_err(threw)?;
    evaluated.await.map_err(threw)?;
    if read_text(runtime, "String(globalThis.__atomicMissing)", 8, false)?.as_deref()
        == Some("true")
    {
        return Err(Failure::Refused(format!(
            "{export} is not an exported function"
        )));
    }
    match read_text(
        runtime,
        "globalThis.__atomicError",
        limits.error_bytes,
        controls.text_limits,
    ) {
        Ok(Some(error)) => return Err(Failure::Threw(error)),
        Ok(None) => {}
        Err(Failure::OutputLimit) => return Err(Failure::ErrorLimit),
        Err(other) => return Err(other),
    }
    let output = read_text(
        runtime,
        "globalThis.__atomicResult",
        limits.output_bytes,
        controls.text_limits,
    )?
    .ok_or_else(|| Failure::NotJson("the helper returned undefined".into()))?;
    serde_json::from_str(&output).map_err(|error| Failure::NotJson(error.to_string()))
}

/// Calls one export of `captured` in a fresh isolate on its own thread, so no
/// module state survives between calls and the caller's thread may be inside
/// an async runtime. `args` is the JSON array of arguments.
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
    std::thread::scope(|scope| {
        scope
            .spawn(|| Isolate::new(captured, limits, clock, controls)?.call(export, args))
            .join()
            .unwrap_or_else(|_| Err(Failure::Host("helper thread panicked".into())))
    })
}
