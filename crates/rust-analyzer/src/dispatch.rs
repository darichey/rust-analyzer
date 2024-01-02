//! See [RequestDispatcher].
use std::{
    fmt::{self, Debug},
    panic,
};

use ide::Cancelled;
use lsp_server::ExtractError;
use serde::{de::DeserializeOwned, Serialize};
use stdx::{panic_context, thread::ThreadIntent};

use crate::{
    global_state::{GlobalState, GlobalStateSnapshot},
    lsp::LspError,
    main_loop::Task,
    version::version,
};

/// A visitor for routing a raw JSON request to an appropriate handler function.
///
/// Most requests are read-only and async and are handled on the threadpool
/// (`on` method).
///
/// Some read-only requests are latency sensitive, and are immediately handled
/// on the main loop thread (`on_sync`). These are typically typing-related
/// requests.
///
/// Some requests modify the state, and are run on the main thread to get
/// `&mut` (`on_sync_mut`).
///
/// Read-only requests are wrapped into `catch_unwind` -- they don't modify the
/// state, so it's OK to recover from their failures.
pub(crate) struct RequestDispatcher<'a> {
    pub(crate) req: Option<lsp_server::Request>,
    pub(crate) global_state: &'a mut GlobalState,
}

impl RequestDispatcher<'_> {
    /// Dispatches the request onto the current thread, given full access to
    /// mutable global state. Unlike all other methods here, this one isn't
    /// guarded by `catch_unwind`, so, please, don't make bugs :-)
    pub(crate) fn on_sync_mut<R>(
        &mut self,
        f: fn(&mut GlobalState, R::Params) -> anyhow::Result<R::Result>,
    ) -> &mut Self
    where
        R: lsp_types::request::Request,
        R::Params: DeserializeOwned + panic::UnwindSafe + fmt::Debug,
        R::Result: Serialize,
    {
        let (req, params, panic_context) = match self.parse::<R>() {
            Some(it) => it,
            None => return self,
        };
        let _guard = tracing::span!(tracing::Level::INFO, "request", method = ?req.method, "request_id" = ?req.id).entered();
        tracing::debug!(?params);
        let _pctx = stdx::panic_context::enter(panic_context);
        let result = f(self.global_state, params);
        if let Ok(response) = result_to_response::<R>(req.id, result) {
            self.global_state.respond(response);
        }

        self
    }

    /// Dispatches the request onto the current thread.
    pub(crate) fn on_sync<R>(
        &mut self,
        f: fn(GlobalStateSnapshot, R::Params) -> anyhow::Result<R::Result>,
    ) -> &mut Self
    where
        R: lsp_types::request::Request,
        R::Params: DeserializeOwned + panic::UnwindSafe + fmt::Debug,
        R::Result: Serialize,
    {
        let (req, params, panic_context) = match self.parse::<R>() {
            Some(it) => it,
            None => return self,
        };
        let _guard = tracing::span!(tracing::Level::INFO, "request", method = ?req.method, "request_id" = ?req.id).entered();
        tracing::debug!(?params);
        let global_state_snapshot = self.global_state.snapshot();

        let _pctx = stdx::panic_context::enter(panic_context);
        let result = panic::catch_unwind(move || f(global_state_snapshot, params));

        if let Ok(response) = thread_result_to_response::<R>(req.id, result) {
            self.global_state.respond(response);
        }

        self
    }

    /// Dispatches a non-latency-sensitive request onto the thread pool
    /// without retrying it if it panics.
    pub(crate) fn on_no_retry<R>(
        &mut self,
        f: fn(GlobalStateSnapshot, R::Params) -> anyhow::Result<R::Result>,
    ) -> &mut Self
    where
        R: lsp_types::request::Request + 'static,
        R::Params: DeserializeOwned + panic::UnwindSafe + Send + fmt::Debug,
        R::Result: Serialize,
    {
        let (req, params, panic_context) = match self.parse::<R>() {
            Some(it) => it,
            None => return self,
        };

        self.global_state.task_pool.handle.spawn(ThreadIntent::Worker, {
            let world = self.global_state.snapshot();
            move || {
                let _pctx = stdx::panic_context::enter(panic_context);
                let result = panic::catch_unwind(move || f(world, params));
                match thread_result_to_response::<R>(req.id.clone(), result) {
                    Ok(response) => Task::Response(response),
                    Err(_) => Task::Response(lsp_server::Response::new_err(
                        req.id,
                        lsp_server::ErrorCode::ContentModified as i32,
                        "content modified".to_owned(),
                    )),
                }
            }
        });

        self
    }

    /// Dispatches a non-latency-sensitive request onto the thread pool.
    pub(crate) fn on<R>(
        &mut self,
        f: fn(GlobalStateSnapshot, R::Params) -> anyhow::Result<R::Result>,
    ) -> &mut Self
    where
        R: lsp_types::request::Request + 'static,
        R::Params: DeserializeOwned + panic::UnwindSafe + Send + fmt::Debug,
        R::Result: Serialize,
    {
        self.on_with_thread_intent::<true, R>(ThreadIntent::Worker, f)
    }

    /// Dispatches a latency-sensitive request onto the thread pool.
    pub(crate) fn on_latency_sensitive<R>(
        &mut self,
        f: fn(GlobalStateSnapshot, R::Params) -> anyhow::Result<R::Result>,
    ) -> &mut Self
    where
        R: lsp_types::request::Request + 'static,
        R::Params: DeserializeOwned + panic::UnwindSafe + Send + fmt::Debug,
        R::Result: Serialize,
    {
        self.on_with_thread_intent::<true, R>(ThreadIntent::LatencySensitive, f)
    }

    /// Formatting requests should never block on waiting a for task thread to open up, editors will wait
    /// on the response and a late formatting update might mess with the document and user.
    /// We can't run this on the main thread though as we invoke rustfmt which may take arbitrary time to complete!
    pub(crate) fn on_fmt_thread<R>(
        &mut self,
        f: fn(GlobalStateSnapshot, R::Params) -> anyhow::Result<R::Result>,
    ) -> &mut Self
    where
        R: lsp_types::request::Request + 'static,
        R::Params: DeserializeOwned + panic::UnwindSafe + Send + fmt::Debug,
        R::Result: Serialize,
    {
        self.on_with_thread_intent::<false, R>(ThreadIntent::LatencySensitive, f)
    }

    pub(crate) fn finish(&mut self) {
        if let Some(req) = self.req.take() {
            tracing::error!("unknown request: {:?}", req);
            let response = lsp_server::Response::new_err(
                req.id,
                lsp_server::ErrorCode::MethodNotFound as i32,
                "unknown request".to_owned(),
            );
            self.global_state.respond(response);
        }
    }

    fn on_with_thread_intent<const MAIN_POOL: bool, R>(
        &mut self,
        intent: ThreadIntent,
        f: fn(GlobalStateSnapshot, R::Params) -> anyhow::Result<R::Result>,
    ) -> &mut Self
    where
        R: lsp_types::request::Request + 'static,
        R::Params: DeserializeOwned + panic::UnwindSafe + Send + fmt::Debug,
        R::Result: Serialize,
    {
        let (req, params, panic_context) = match self.parse::<R>() {
            Some(it) => it,
            None => return self,
        };
        let _guard = tracing::span!(tracing::Level::INFO, "request", method = ?req.method, "request_id" = ?req.id).entered();
        tracing::debug!(?params);

        let world = self.global_state.snapshot();
        if MAIN_POOL {
            &mut self.global_state.task_pool.handle
        } else {
            &mut self.global_state.fmt_pool.handle
        }
        .spawn(intent, move || {
            let _pctx = stdx::panic_context::enter(panic_context);
            let result = panic::catch_unwind(move || f(world, params));
            match thread_result_to_response::<R>(req.id.clone(), result) {
                Ok(response) => Task::Response(response),
                Err(_) => Task::Retry(req),
            }
        });

        self
    }

    fn parse<R>(&mut self) -> Option<(lsp_server::Request, R::Params, String)>
    where
        R: lsp_types::request::Request,
        R::Params: DeserializeOwned + fmt::Debug,
    {
        let req = match &self.req {
            Some(req) if req.method == R::METHOD => self.req.take()?,
            _ => return None,
        };

        let res = crate::from_json(R::METHOD, &req.params);
        match res {
            Ok(params) => {
                let panic_context =
                    format!("\nversion: {}\nrequest: {} {params:#?}", version(), R::METHOD);
                Some((req, params, panic_context))
            }
            Err(err) => {
                let response = lsp_server::Response::new_err(
                    req.id,
                    lsp_server::ErrorCode::InvalidParams as i32,
                    err.to_string(),
                );
                self.global_state.respond(response);
                None
            }
        }
    }
}

fn thread_result_to_response<R>(
    id: lsp_server::RequestId,
    result: Result<anyhow::Result<R::Result>, Box<dyn std::any::Any + Send + 'static>>,
) -> Result<lsp_server::Response, Cancelled>
where
    R: lsp_types::request::Request,
    R::Params: DeserializeOwned,
    R::Result: Serialize,
{
    match result {
        Ok(result) => result_to_response::<R>(id, result),
        Err(panic) => {
            let mut message = "request handler panicked".to_owned();

            let panic_message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied());

            if let Some(panic_message) = panic_message {
                message.push_str(": ");
                message.push_str(panic_message);
                message.push('\n');
            };

            panic_context::with_backtrace(|backtrace| {
                if let Some(backtrace) = backtrace {
                    message.push_str("backtrace:\n");
                    message.push_str(&backtrace.to_string())
                }
            });

            Ok(lsp_server::Response::new_err(
                id,
                lsp_server::ErrorCode::InternalError as i32,
                message,
            ))
        }
    }
}

fn result_to_response<R>(
    id: lsp_server::RequestId,
    result: anyhow::Result<R::Result>,
) -> Result<lsp_server::Response, Cancelled>
where
    R: lsp_types::request::Request,
    R::Params: DeserializeOwned,
    R::Result: Serialize,
{
    let res = match result {
        Ok(resp) => lsp_server::Response::new_ok(id, &resp),
        Err(e) => match e.downcast::<LspError>() {
            Ok(lsp_error) => lsp_server::Response::new_err(id, lsp_error.code, lsp_error.message),
            Err(e) => match e.downcast::<Cancelled>() {
                Ok(cancelled) => return Err(cancelled),
                Err(e) => lsp_server::Response::new_err(
                    id,
                    lsp_server::ErrorCode::InternalError as i32,
                    e.to_string(),
                ),
            },
        },
    };
    Ok(res)
}

pub(crate) struct NotificationDispatcher<'a> {
    pub(crate) not: Option<lsp_server::Notification>,
    pub(crate) global_state: &'a mut GlobalState,
}

impl NotificationDispatcher<'_> {
    pub(crate) fn on_sync_mut<N>(
        &mut self,
        f: fn(&mut GlobalState, N::Params) -> anyhow::Result<()>,
    ) -> anyhow::Result<&mut Self>
    where
        N: lsp_types::notification::Notification,
        N::Params: DeserializeOwned + Send + Debug,
    {
        let not = match self.not.take() {
            Some(it) => it,
            None => return Ok(self),
        };

        let _guard =
            tracing::span!(tracing::Level::INFO, "notification", method = ?not.method).entered();

        let params = match not.extract::<N::Params>(N::METHOD) {
            Ok(it) => it,
            Err(ExtractError::JsonError { method, error }) => {
                panic!("Invalid request\nMethod: {method}\n error: {error}",)
            }
            Err(ExtractError::MethodMismatch(not)) => {
                self.not = Some(not);
                return Ok(self);
            }
        };

        tracing::debug!(?params);

        let _pctx = stdx::panic_context::enter(format!(
            "\nversion: {}\nnotification: {}",
            version(),
            N::METHOD
        ));
        f(self.global_state, params)?;
        Ok(self)
    }

    pub(crate) fn finish(&mut self) {
        if let Some(not) = &self.not {
            if !not.method.starts_with("$/") {
                tracing::error!("unhandled notification: {:?}", not);
            }
        }
    }
}
