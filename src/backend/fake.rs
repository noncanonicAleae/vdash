//! Scripted backend for tests.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use super::{AgentCall, AgentResult, Backend, Failure};
use crate::config::{BackendKind, Role};
use crate::paths::Ctx;

pub type StepFn = Box<dyn Fn(&AgentCall) -> Value + Send + Sync>;

pub enum Step {
    Reply(Value),
    Func(StepFn),
    Fail(Failure),
}

#[derive(Debug, Clone)]
pub struct Call {
    pub role: Role,
    pub prompt: String,
    pub model: Option<String>,
}

pub struct FakeBackend {
    kind: BackendKind,
    script: Mutex<BTreeMap<Role, VecDeque<Step>>>,
    pub calls: Mutex<Vec<Call>>,
    pub delay: Duration,
    /// When a role's queue is empty: reply with this instead of panicking.
    pub fallback: Mutex<BTreeMap<Role, Value>>,
}

impl FakeBackend {
    pub fn new(kind: BackendKind) -> Self {
        FakeBackend {
            kind,
            script: Mutex::new(BTreeMap::new()),
            calls: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
            fallback: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn with_delay(mut self, d: Duration) -> Self {
        self.delay = d;
        self
    }

    pub fn push(&self, role: Role, step: Step) -> &Self {
        self.script.lock().unwrap().entry(role).or_default().push_back(step);
        self
    }

    pub fn reply(&self, role: Role, v: Value) -> &Self {
        self.push(role, Step::Reply(v))
    }

    pub fn func(&self, role: Role, f: impl Fn(&AgentCall) -> Value + Send + Sync + 'static) -> &Self {
        self.push(role, Step::Func(Box::new(f)))
    }

    pub fn fail(&self, role: Role, f: Failure) -> &Self {
        self.push(role, Step::Fail(f))
    }

    pub fn always(&self, role: Role, v: Value) -> &Self {
        self.fallback.lock().unwrap().insert(role, v);
        self
    }

    pub fn count(&self, role: Role) -> usize {
        self.calls.lock().unwrap().iter().filter(|c| c.role == role).count()
    }
}

impl Backend for FakeBackend {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn run(&self, _ctx: &Ctx, call: &AgentCall) -> AgentResult {
        self.calls.lock().unwrap().push(Call {
            role: call.role,
            prompt: call.prompt.clone(),
            model: call.model.clone(),
        });
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        let step = self.script.lock().unwrap().get_mut(&call.role).and_then(|q| q.pop_front());
        let mut r = AgentResult {
            backend: Some(self.kind),
            model: call.model.clone(),
            argv: vec![format!("fake-{}", self.kind)],
            seconds: self.delay.as_secs_f64(),
            cost_usd: Some(0.0),
            ..Default::default()
        };
        let value = match step {
            Some(Step::Reply(v)) => v,
            Some(Step::Func(f)) => f(call),
            Some(Step::Fail(f)) => {
                r.failure = Some(f);
                return r;
            }
            None => match self.fallback.lock().unwrap().get(&call.role) {
                Some(v) => v.clone(),
                None => panic!("FakeBackend({}): no scripted reply left for role {}", self.kind, call.role),
            },
        };
        r.text = value.to_string();
        r.raw = r.text.clone();
        r.structured = Some(value);
        r
    }
}
