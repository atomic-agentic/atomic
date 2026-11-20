// SPDX-License-Identifier: Apache-2.0

//! Spans held so that a stopping process can end them all.
//!
//! A tracing span ends when its last handle is dropped. A process that stops
//! mid-run exits without dropping the futures that hold its spans (the
//! workflow's, a step's, a model call's), so those would never end, and
//! never be exported. Every long-lived handle atomic keeps is [`Held`]
//! here instead: a future runs inside it through [`Held::run`], which enters
//! the span only while it polls, and [`end_all`] drops every handle at once.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

type Slot = Mutex<Option<tracing::Span>>;

/// Every held span's slot, to end at a stop.
static OPEN: Mutex<Vec<Weak<Slot>>> = Mutex::new(Vec::new());

/// A span that ends when the last clone of this is dropped, when
/// [`Held::end`] is called, or at [`end_all`], whichever comes first.
#[derive(Clone)]
pub struct Held(Arc<Slot>);

/// Hold `span` (see [`Held`]).
pub fn hold(span: tracing::Span) -> Held {
    let slot = Arc::new(Mutex::new(Some(span)));
    if let Ok(mut open) = OPEN.lock() {
        // Kept short: a slot whose holders are gone is dropped here.
        if open.len() >= 64 {
            open.retain(|w| w.strong_count() > 0);
        }
        open.push(Arc::downgrade(&slot));
    }
    Held(slot)
}

impl Held {
    /// A handle to the span while it is held, to record on; drop it soon.
    pub fn span(&self) -> Option<tracing::Span> {
        self.0.lock().ok().and_then(|s| s.clone())
    }

    /// End the span now.
    pub fn end(&self) {
        if let Ok(mut slot) = self.0.lock() {
            slot.take();
        }
    }

    /// Run `future` inside the span, entered only while it polls.
    pub fn run<F: Future>(&self, future: F) -> InHeld<F> {
        InHeld {
            inner: Box::pin(future),
            held: self.clone(),
        }
    }
}

/// A future run inside a [`Held`] span.
pub struct InHeld<F> {
    inner: Pin<Box<F>>,
    held: Held,
}

impl<F> InHeld<F> {
    /// The span it runs in.
    pub fn held(&self) -> &Held {
        &self.held
    }
}

impl<F: Future> Future for InHeld<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.held.span() {
            Some(span) => {
                let _entered = span.enter();
                self.inner.as_mut().poll(cx)
            }
            None => self.inner.as_mut().poll(cx),
        }
    }
}

/// End every span still held, marking each `atomic.stopped`: they are
/// exported with what they recorded so far. Call it as the process stops.
pub fn end_all() {
    let open: Vec<_> = OPEN
        .lock()
        .map(|mut o| std::mem::take(&mut *o))
        .unwrap_or_default();
    for slot in open.iter().filter_map(Weak::upgrade) {
        if let Some(span) = slot.lock().ok().and_then(|mut s| s.take()) {
            span.record("atomic.stopped", true);
        }
    }
}
