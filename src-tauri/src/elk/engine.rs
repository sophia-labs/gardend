//! The ELK engine thread: `vendor/elkjs/elk.bundled.js` (unmodified) on QuickJS.
//!
//! One thread, named `elk`, started on first use, owns the QuickJS runtime (a runtime is not `Send`, so it never
//! leaves the thread). Jobs arrive on a channel and run one at a time. elkjs talks to its "worker" through
//! `setTimeout` and promises; the shim below queues the timers, and the job loop drains them and QuickJS's job queue
//! until the layout answers. The runtime has a memory limit and an interrupt handler that stops a job past its
//! deadline. After any failure (an exception, an interrupt, running out of memory) the runtime is dropped and a fresh
//! one boots for the next job, so one bad graph never poisons the next.

use rquickjs::{CatchResultExt, Context, Function, Runtime};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const ELK_JS: &str = include_str!("../../vendor/elkjs/elk.bundled.js");
const MEMORY_LIMIT: usize = 512 * 1024 * 1024;
const STACK_LIMIT: usize = 8 * 1024 * 1024;

/// What elkjs expects of its host: a `global` (so its GWT code finds `$wnd`), a silent `console`, and `setTimeout`.
const SHIM: &str = r#"
var global = globalThis;
var __noop = function () {};
globalThis.console = { log: __noop, info: __noop, warn: __noop, error: __noop, debug: __noop };
var __timers = [];
globalThis.setTimeout = function (f) { __timers.push(f); return __timers.length; };
globalThis.clearTimeout = __noop;
globalThis.__drain = function () { var n = 0; while (__timers.length) { var f = __timers.shift(); f(); n++; } return n; };
"#;

const DRIVER: &str = r#"
globalThis.__elk = new ELK();
globalThis.__start = function (json) {
  globalThis.__out = undefined;
  __elk.layout(JSON.parse(json)).then(
    function (g) { globalThis.__out = JSON.stringify({ ok: g }); },
    function (e) { globalThis.__out = JSON.stringify({ error: String((e && e.message) || e) }); }
  );
};
"#;

type Reply = Result<Value, String>;

struct Job {
    graph: Value,
    deadline: Duration,
    reply: tokio::sync::oneshot::Sender<Reply>,
}

/// Queue a layout on the engine thread (starting it on first use). The receiver gets the laid-out graph.
pub(super) fn submit(
    graph: Value,
    deadline: Duration,
) -> Result<tokio::sync::oneshot::Receiver<Reply>, String> {
    static QUEUE: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("elk".into())
            .spawn(move || serve(rx))
            .expect("spawn the elk layout thread");
        Mutex::new(tx)
    });
    let (reply, answer) = tokio::sync::oneshot::channel();
    queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .send(Job {
            graph,
            deadline,
            reply,
        })
        .map_err(|_| "layout: the layout engine stopped".to_string())?;
    Ok(answer)
}

fn serve(jobs: mpsc::Receiver<Job>) {
    let mut engine: Option<Engine> = None;
    for job in jobs {
        let result = (|| {
            if engine.is_none() {
                engine = Some(Engine::boot()?);
            }
            engine
                .as_mut()
                .expect("booted")
                .run(&job.graph, job.deadline)
        })();
        if result.is_err() {
            // A failed job may leave the runtime mid-promise or out of memory: start clean next time.
            engine = None;
        }
        let _ = job.reply.send(result);
    }
}

struct Engine {
    runtime: Runtime,
    context: Context,
    /// Nanoseconds since `epoch` after which the running job is interrupted; 0 = no deadline.
    deadline: Arc<AtomicU64>,
    epoch: Instant,
}

impl Engine {
    fn boot() -> Result<Self, String> {
        let runtime = Runtime::new().map_err(|e| format!("layout: start QuickJS: {e}"))?;
        runtime.set_memory_limit(MEMORY_LIMIT);
        runtime.set_max_stack_size(STACK_LIMIT);
        let deadline = Arc::new(AtomicU64::new(0));
        let epoch = Instant::now();
        {
            let deadline = deadline.clone();
            runtime.set_interrupt_handler(Some(Box::new(move || {
                let at = deadline.load(Ordering::Relaxed);
                at != 0 && epoch.elapsed().as_nanos() as u64 > at
            })));
        }
        let context = Context::full(&runtime).map_err(|e| format!("layout: start QuickJS: {e}"))?;
        context.with(|ctx| -> Result<(), String> {
            for (name, source) in [("shim", SHIM), ("elkjs", ELK_JS), ("driver", DRIVER)] {
                ctx.eval::<(), _>(source)
                    .catch(&ctx)
                    .map_err(|e| format!("layout: load {name}: {e}"))?;
            }
            Ok(())
        })?;
        Ok(Self {
            runtime,
            context,
            deadline,
            epoch,
        })
    }

    fn run(&mut self, graph: &Value, deadline: Duration) -> Result<Value, String> {
        let json = serde_json::to_string(graph).map_err(|e| format!("layout: {e}"))?;
        let until = self.epoch.elapsed() + deadline;
        self.deadline
            .store(until.as_nanos().max(1) as u64, Ordering::Relaxed);
        let result = self.drive(&json);
        self.deadline.store(0, Ordering::Relaxed);
        let too_long = self.epoch.elapsed() > until;
        match result {
            Err(_) if too_long => Err(format!(
                "layout took too long (over {} ms); lay out fewer shapes",
                deadline.as_millis()
            )),
            other => other,
        }
    }

    fn drive(&self, json: &str) -> Result<Value, String> {
        self.context.with(|ctx| -> Result<(), String> {
            let start: Function = ctx
                .globals()
                .get("__start")
                .map_err(|e| format!("layout: {e}"))?;
            start
                .call::<_, ()>((json,))
                .catch(&ctx)
                .map_err(|e| format!("layout: {e}"))
        })?;
        loop {
            let drained = self.context.with(|ctx| {
                ctx.eval::<i32, _>("__drain()")
                    .catch(&ctx)
                    .map_err(|e| format!("layout: {e}"))
            })?;
            let mut jobs = 0;
            loop {
                match self.runtime.execute_pending_job() {
                    Ok(true) => jobs += 1,
                    Ok(false) => break,
                    Err(_) => return Err("layout: ELK failed inside a promise job".into()),
                }
            }
            let out: Option<String> = self.context.with(|ctx| {
                ctx.globals()
                    .get::<_, Option<String>>("__out")
                    .map_err(|e| format!("layout: {e}"))
            })?;
            if let Some(out) = out {
                let mut answer: Value =
                    serde_json::from_str(&out).map_err(|e| format!("layout: {e}"))?;
                if let Some(error) = answer.get("error").and_then(Value::as_str) {
                    return Err(format!("layout: ELK refused the graph: {error}"));
                }
                return answer
                    .get_mut("ok")
                    .map(Value::take)
                    .ok_or_else(|| "layout: ELK gave no graph".to_string());
            }
            if drained == 0 && jobs == 0 {
                return Err("layout: ELK stopped without an answer".into());
            }
        }
    }
}
