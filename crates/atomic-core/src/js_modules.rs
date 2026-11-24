// SPDX-License-Identifier: Apache-2.0
//! A closed module graph. The engine has no filesystem module loader.
use boa_engine::{
    Context, JsNativeError, JsResult, Module,
    builtins::promise::PromiseState,
    module::{ModuleLoader, ModuleRequest, Referrer},
};
use std::path::Path;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

#[derive(Default)]
pub(crate) struct Modules {
    entries: RefCell<BTreeMap<String, Module>>,
    linking: Cell<bool>,
}

impl Modules {
    pub(crate) fn insert(&self, name: String, module: Module) {
        self.entries.borrow_mut().insert(name, module);
    }

    pub(crate) fn evaluate(&self, module: &Module, context: &mut Context) -> JsResult<()> {
        self.linking.set(true);
        let loading = module.load(context);
        let jobs = context.run_jobs();
        self.linking.set(false);
        jobs?;
        settled(loading.state())?;
        module.link(context)?;
        let evaluated = module.evaluate(context)?;
        context.run_jobs()?;
        settled(evaluated.state())
    }
}

fn settled(state: PromiseState) -> JsResult<()> {
    match state {
        PromiseState::Rejected(reason) => Err(boa_engine::JsError::from_opaque(reason)),
        PromiseState::Pending => Err(JsNativeError::typ()
            .with_message("module evaluation did not settle")
            .into()),
        PromiseState::Fulfilled(_) => Ok(()),
    }
}

impl ModuleLoader for Modules {
    fn init_import_meta(
        self: Rc<Self>,
        meta: &boa_engine::JsObject,
        module: &Module,
        ctx: &mut Context,
    ) {
        if let Some(path) = module.path().and_then(Path::to_str) {
            let _ = meta.create_data_property(
                boa_engine::js_string!("url"),
                boa_engine::JsString::from(path),
                ctx,
            );
        }
    }
    async fn load_imported_module(
        self: Rc<Self>,
        referrer: Referrer,
        request: ModuleRequest,
        _: &RefCell<&mut Context>,
    ) -> JsResult<Module> {
        if !self.linking.get() {
            return Err(JsNativeError::typ()
                .with_message("dynamic imports are disabled")
                .into());
        }
        let mut name = request.specifier().to_std_string_escaped();
        if (name.starts_with("./") || name.starts_with("../"))
            && let Some(base) = referrer.path().and_then(Path::to_str)
            && base.starts_with("atomic-helper:///")
        {
            name = url::Url::parse(base)
                .and_then(|base| base.join(&name))
                .map_err(|_| JsNativeError::typ().with_message("invalid helper import URL"))?
                .to_string();
        }
        self.entries.borrow().get(&name).cloned().ok_or_else(|| {
            JsNativeError::typ()
                .with_message(format!("uncaptured module: {name}"))
                .into()
        })
    }
}
