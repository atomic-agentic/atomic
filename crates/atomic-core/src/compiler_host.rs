// SPDX-License-Identifier: Apache-2.0
//! The embedded authoring worker runs in a supervised child using Boa.
use crate::js_modules::Modules;
use boa_engine::{
    Context, JsArgs, JsError, JsNativeError, JsResult, JsString, JsValue, Module, NativeFunction,
    Source,
    object::builtins::{JsArrayBuffer, JsUint8Array},
};
use std::{
    io::{Read, Write},
    rc::Rc,
};

mod astro_parser;
mod limits;
#[cfg(test)]
mod tests;
mod typescript;

/// The argument that makes the atomic binary serve one compiler call.
pub const CHILD_ARG: &str = "__atomic-compiler";

// Statics, not consts, so the binary holds one copy of each wherever they
// are used.
/// The bundled worker, built from `authoring/src`.
pub(crate) static WORKER: &str = include_str!("../assets/worker.mjs");
/// What the worker may use of Deno, Node and the web platform.
pub(crate) const HOST: &str = include_str!("compiler_host/host.js");

/// The engine heap one compile may use.
pub(crate) const HEAP_LIMIT: usize = 1024 * 1024 * 1024;

/// The Node modules the worker imports, served from the host.
const NODE_MODULES: [&str; 4] = ["fs/promises", "path", "crypto", "url"];

/// The command that starts a compiler child: this atomic binary, with
/// [`CHILD_ARG`] and no environment.
pub(crate) fn command() -> Result<crate::helper_runtime::HostCommand, String> {
    Ok(crate::helper_runtime::HostCommand {
        program: std::env::current_exe()
            .map_err(|e| format!("cannot find the atomic binary to run the Astro compiler: {e}"))?,
        args: vec![CHILD_ARG.to_owned()],
        env: Vec::new(),
    })
}

/// Dispatch the private engine modes before starting the normal CLI.
pub fn dispatch_embedded_child() {
    let arg = std::env::args_os().nth(1);
    if arg.as_deref() == Some(CHILD_ARG.as_ref()) {
        std::process::exit(serve());
    }
    if arg.as_deref() == Some(crate::helper_runtime::CHILD_ARG.as_ref()) {
        std::process::exit(crate::helper_runtime::serve());
    }
    crate::js_budget::ordinary_process();
}

/// Serves one compiler call on stdin and stdout and returns the exit status:
/// the worker's own, 0 when it finishes, and 1 with the reason on stderr
/// when it fails to start, throws or exceeds the heap limit.
pub fn serve() -> i32 {
    serve_module(WORKER, HEAP_LIMIT)
}

fn serve_module(source: &str, heap_limit: usize) -> i32 {
    // The memory limit follows the production heap limit whatever this
    // child's heap, so that a test's smaller heap is what stops it.
    let status = match limits::limit_self(limits::memory_limit(heap_limit)).and_then(|()| {
        #[cfg(test)]
        tests::shorten_cpu_probe()?;
        run(source, heap_limit)
    }) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("{message}");
            1
        }
    };
    let _ = std::io::stdout().flush();
    status
}

// A test binary that links atomic-core serves compiler calls the way the
// atomic binary's `main` does, so tests that compile atoms in-process start
// the same child. It runs before libtest's `main`, which would read the
// argument as a test filter and print to stdout. Never in a release build:
// only in atomic-core's own tests and with the dev-only feature.
#[cfg(any(test, feature = "compiler-in-tests"))]
#[allow(unsafe_code)]
mod test_dispatch {
    use super::*;

    /// release-package.sh refuses a binary holding this text. It is split
    /// here because this file's own text is in every binary (`COMPILER_HASH`).
    static TEST_DISPATCH: &str = concat!("ATOMIC_COMPILER_", "TEST_DISPATCH");

    #[ctor::ctor(unsafe)]
    fn serve_in_a_test_binary() {
        if std::env::args_os().nth(1).as_deref() == Some(CHILD_ARG.as_ref()) {
            std::hint::black_box(TEST_DISPATCH);
            #[cfg(test)]
            if let Some(probe) = std::env::var_os(tests::PROBE_ENV) {
                let heap = std::env::var(tests::HEAP_ENV)
                    .ok()
                    .and_then(|heap| heap.parse().ok())
                    .unwrap_or(HEAP_LIMIT);
                std::process::exit(serve_module(&probe.to_string_lossy(), heap));
            }
            std::process::exit(serve());
        }
    }
}

/// A Node module as the worker imports it: the default export, and each
/// member as a named export.
fn node_source(name: &str) -> String {
    let members: &[&str] = match name {
        "fs/promises" => &[
            "readFile",
            "realpath",
            "writeFile",
            "mkdir",
            "readdir",
            "rm",
            "open",
            "link",
            "stat",
            "lstat",
            "access",
        ],
        "path" => &[
            "sep",
            "delimiter",
            "isAbsolute",
            "resolve",
            "normalize",
            "join",
            "relative",
            "dirname",
            "basename",
            "extname",
            "posix",
        ],
        "crypto" => &["createHash"],
        _ => &["fileURLToPath"],
    };
    let mut source = format!(
        "const module = globalThis.__atomicCompilerNode[{}];\nexport default module;\n",
        serde_json::Value::from(name)
    );
    for member in members {
        source.push_str(&format!("export const {member} = module.{member};\n"));
    }
    source
}

fn run(source: &str, heap_limit: usize) -> Result<(), String> {
    crate::js_budget::enter(heap_limit, heap_limit);
    let modules = Rc::new(Modules::default());
    let mut context = Context::builder()
        .module_loader(modules.clone())
        .build()
        .map_err(|e| e.to_string())?;
    let result = (|| -> JsResult<()> {
        for (name, arity, op) in [
            (
                "op_compiler_parse_astro",
                1,
                parse as boa_engine::native_function::NativeFunctionPointer,
            ),
            ("op_compiler_typescript", 4, typescript::op),
            ("op_compiler_tsx", 2, tsx),
            ("op_compiler_read", 1, read),
            ("op_compiler_write", 1, write),
            ("op_compiler_sha256", 1, hash),
            ("op_compiler_encode", 1, encode),
            ("op_compiler_decode", 1, decode),
            ("op_compiler_exit", 1, exit),
            ("op_compiler_print", 1, print),
            ("op_compiler_random", 1, random),
            ("op_compiler_timer", 2, timer),
        ] {
            context.register_global_builtin_callable(
                JsString::from(name),
                arity,
                NativeFunction::from_fn_ptr(op),
            )?;
        }
        context.eval(Source::from_bytes(&format!("{HOST}();")))?;
        for name in NODE_MODULES {
            let module = Module::parse(Source::from_bytes(&node_source(name)), None, &mut context)?;
            modules.insert(format!("node:{name}"), module);
        }
        let worker = Module::parse(
            Source::from_bytes(source)
                .with_path(std::path::Path::new("atomic-compiler:///worker.mjs")),
            None,
            &mut context,
        )?;
        modules.evaluate(&worker, &mut context)
    })();
    result.map_err(|e| format!("the Astro compiler failed: {}", error_text(e, &mut context)))
}

pub(crate) fn error_text(error: JsError, context: &mut Context) -> String {
    error
        .into_opaque(context)
        .and_then(|v| v.to_string(context))
        .map(|v| v.to_std_string_lossy())
        .unwrap_or_else(|_| "JavaScript exception".into())
}

fn string(args: &[JsValue], n: usize, ctx: &mut Context) -> JsResult<String> {
    Ok(args
        .get_or_undefined(n)
        .to_string(ctx)?
        .to_std_string_lossy())
}
fn text(s: &str) -> JsValue {
    JsString::from(s).into()
}
fn parse(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(text(
        &astro_parser::parse(&string(args, 0, ctx)?).to_string(),
    ))
}
fn tsx(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(text(
        &astro_parser::tsx(&string(args, 0, ctx)?, &string(args, 1, ctx)?).to_string(),
    ))
}
fn bytes(
    args: &[JsValue],
    ctx: &mut Context,
    f: impl FnOnce(&mut [u8]) -> JsResult<JsValue>,
) -> JsResult<JsValue> {
    let invalid = || JsNativeError::typ().with_message("invalid byte view");
    let view =
        JsUint8Array::from_object(args.get_or_undefined(0).as_object().ok_or_else(invalid)?)?;
    let start = view.byte_offset(ctx)?;
    let end = start
        .checked_add(view.byte_length(ctx)?)
        .ok_or_else(invalid)?;
    let buffer = JsArrayBuffer::from_object(view.buffer(ctx)?.as_object().ok_or_else(invalid)?)?;
    let mut data = buffer.data_mut().ok_or_else(invalid)?;
    f(data.get_mut(start..end).ok_or_else(invalid)?)
}
fn read(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    bytes(args, ctx, |buf| {
        Ok(match std::io::stdin().lock().read(buf) {
            Ok(0) if !buf.is_empty() => -1,
            Ok(n) => i32::try_from(n).unwrap_or(-2),
            Err(_) => -2,
        }
        .into())
    })
}
fn write(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    bytes(args, ctx, |buf| {
        let mut out = std::io::stdout().lock();
        Ok(out
            .write(buf)
            .and_then(|n| out.flush().map(|_| n))
            .ok()
            .and_then(|n| i32::try_from(n).ok())
            .unwrap_or(-1)
            .into())
    })
}
fn hash(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    use sha2::{Digest, Sha256};
    bytes(args, ctx, |buf| {
        Ok(text(&format!("{:x}", Sha256::digest(buf))))
    })
}
fn encode(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsUint8Array::from_iter(string(args, 0, ctx)?.bytes(), ctx)?.into())
}
fn decode(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    bytes(args, ctx, |buf| {
        Ok(text(&String::from_utf8_lossy(
            buf.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(buf),
        )))
    })
}
fn exit(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    std::process::exit(args.get_or_undefined(0).to_i32(ctx)?)
}
fn print(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    eprint!("{}", string(args, 0, ctx)?);
    Ok(JsValue::undefined())
}
fn random(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    bytes(args, ctx, |buf| {
        getrandom::fill(buf)
            .map_err(|_| JsNativeError::error().with_message("random bytes unavailable"))?;
        Ok(JsValue::undefined())
    })
}
fn timer(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let f = args
        .get_or_undefined(0)
        .as_callable()
        .ok_or_else(|| JsNativeError::typ().with_message("expected timer callback"))?;
    let delay = args.get_or_undefined(1).to_u32(ctx)?;
    ctx.enqueue_job(
        boa_engine::job::TimeoutJob::from_duration(
            move |ctx| f.call(&JsValue::undefined(), &[], ctx),
            std::time::Duration::from_millis(delay.into()),
        )
        .into(),
    );
    Ok(JsValue::undefined())
}
