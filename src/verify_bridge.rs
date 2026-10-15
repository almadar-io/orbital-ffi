//! `verify-bridge`: the in-app observation and dispatch channel `orb verify`
//! drives against the real iOS Simulator / Android emulator app.
//!
//! Off by default and never compiled into a release shell. At runtime it is
//! inert until the host calls `orbital_verify_bridge_start`. Recording
//! ([`Recorder`]) is pure data — the HTTP server ([`BridgeServer`]) is one
//! background thread over std `TcpListener` on 127.0.0.1.
//!
//! `/send` never dispatches into the kernel: it invokes the host-registered
//! callback so the event takes the SAME path as a tap (server leg + re-render).
//!
//! Field names mirror `packages/almadar-core/src/types/verification.ts`.

use orbital_core::runtime::{InMemoryEntityCache, RuntimeKernel};
use orbital_core::{
    ClientEffect, EntitySource, EventResponse, OrbitalSchema, ProcessEventResult,
    ServerEffectResult, TraitReference, Value as CoreValue,
};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::{c_void, CString};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Explicit bound on the event log; evictions are counted in `dropped`.
pub const EVENT_LOG_CAPACITY: usize = 10_000;
/// Explicit bound on the transition journal; evictions are counted in `transitionsDropped`.
pub const TRANSITION_CAPACITY: usize = 10_000;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const ACCEPT_POLL: Duration = Duration::from_millis(20);

pub use crate::kernel_c_api::VerifyDispatchCallback;

#[derive(Clone, Copy)]
pub struct CallbackSlot {
    pub callback: VerifyDispatchCallback,
    pub ctx: usize,
}

pub type SharedCallback = Arc<Mutex<Option<CallbackSlot>>>;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TraitMeta {
    pub orbital: String,
    pub trait_name: String,
    pub states: Vec<String>,
    pub events: Vec<String>,
}

/// Declared states/events of every inline trait on every orbital of the schema.
pub fn trait_metas(schema: &OrbitalSchema) -> Vec<TraitMeta> {
    schema
        .orbitals
        .iter()
        .flat_map(|orbital| orbital.traits.iter().map(move |t| (orbital, t)))
        .filter_map(|(orbital, t)| match t {
            TraitReference::Inline(def) => {
                let states = def
                    .state_machine
                    .states
                    .iter()
                    .map(|s| s.name.clone())
                    .collect();
                let mut events: Vec<String> = Vec::new();
                let declared = def
                    .state_machine
                    .events
                    .iter()
                    .flatten()
                    .map(|e| e.key.clone());
                let fired = def
                    .state_machine
                    .transitions
                    .iter()
                    .map(|t| t.event.clone());
                for key in declared.chain(fired) {
                    if !events.contains(&key) {
                        events.push(key);
                    }
                }
                Some(TraitMeta {
                    orbital: orbital.name.clone(),
                    trait_name: def.name.clone(),
                    states,
                    events,
                })
            }
            TraitReference::Reference { .. } => None,
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct EffectTrace {
    #[serde(rename = "type")]
    pub effect_type: String,
    #[serde(rename = "entityName", skip_serializing_if = "Option::is_none")]
    pub entity_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<&'static str>,
    pub args: Vec<Value>,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerResponseTrace {
    pub orbital_name: String,
    pub success: bool,
    pub transitioned: bool,
    pub client_effects: usize,
    pub data_entities: BTreeMap<String, usize>,
    pub emitted_events: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransitionTrace {
    pub id: String,
    pub trait_name: String,
    pub from: String,
    pub to: String,
    pub event: String,
    pub effects: Vec<EffectTrace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_response: Option<ServerResponseTrace>,
    /// The tick whose emit started this dispatch (a tick-driven transition).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tick: Option<String>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventLogEntry {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Value>,
    pub timestamp: u64,
}

#[derive(Default)]
struct Journal {
    transitions: VecDeque<TransitionTrace>,
    transitions_dropped: u64,
    events: VecDeque<EventLogEntry>,
    events_dropped: u64,
}

impl Journal {
    fn push_transition(&mut self, trace: TransitionTrace) {
        if self.transitions.len() == TRANSITION_CAPACITY {
            self.transitions.pop_front();
            self.transitions_dropped += 1;
        }
        self.transitions.push_back(trace);
    }

    fn push_event(&mut self, entry: EventLogEntry) {
        if self.events.len() == EVENT_LOG_CAPACITY {
            self.events.pop_front();
            self.events_dropped += 1;
        }
        self.events.push_back(entry);
    }
}

/// What `/events?since=N` returns. `next` is the absolute cursor to pass back;
/// `dropped` is how many leading entries were evicted (a `since` below it lost entries).
#[derive(Debug, Serialize)]
pub struct EventsPage {
    pub entries: Vec<EventLogEntry>,
    pub next: u64,
    pub dropped: u64,
}

pub struct Recorder {
    enabled: AtomicBool,
    pending: AtomicUsize,
    queued_sends: AtomicUsize,
    ready: AtomicBool,
    /// The host's render generation it last folded into its view state, and the last one it
    /// reports drawn: the screen lags the kernel while `painted < folded`.
    folded: AtomicU64,
    painted: AtomicU64,
    seq: AtomicU64,
    traits: Vec<TraitMeta>,
    journal: Mutex<Journal>,
    route: Mutex<Option<String>>,
    tick_fires: Mutex<BTreeMap<(String, String), u64>>,
}

/// Journal event type for one effect outcome the server reported on a settled leg.
pub const SERVER_EFFECT_EVENT: &str = "server-effect";

/// A delivery under the name of an event this response emitted is that one bus event,
/// journaled once with the emits (the web eventLog logs each bus emission once).
fn already_on_the_bus(response: &EventResponse, delivered: &str) -> bool {
    response.emitted_events.iter().any(|e| e.event == delivered)
}

fn saturating_decrement(counter: &AtomicUsize) {
    let _ = counter.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
        Some(n.saturating_sub(1))
    });
}

fn client_effect_trace(effect: &ClientEffect) -> EffectTrace {
    let effect_type = match effect {
        ClientEffect::RenderUi { .. } => "render-ui",
        ClientEffect::Navigate { .. } => "navigate",
        ClientEffect::NavigateBack => "navigate-back",
    };
    EffectTrace {
        effect_type: effect_type.to_string(),
        entity_name: None,
        action: None,
        outcome: None,
        args: Vec::new(),
        status: "executed",
        error: None,
    }
}

fn server_effect_trace(result: &ServerEffectResult) -> EffectTrace {
    let kind = serde_json::to_value(result.effect)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let denied = result.denied == Some(true);
    EffectTrace {
        // `EffectTrace.action` is the persist mutation (`create|update|delete|batch`); other
        // effects carry their own detail in `action` (a `set` target) that is not one.
        action: result.action.clone().filter(|_| kind == "persist"),
        effect_type: kind,
        entity_name: result.entity_type.clone(),
        outcome: Some(if denied {
            "denied"
        } else if result.success {
            "success"
        } else {
            "failed"
        }),
        args: Vec::new(),
        status: if result.success { "executed" } else { "failed" },
        error: result.error.clone(),
    }
}

impl Recorder {
    pub fn new(traits: Vec<TraitMeta>) -> Self {
        Self {
            enabled: AtomicBool::new(false),
            pending: AtomicUsize::new(0),
            queued_sends: AtomicUsize::new(0),
            ready: AtomicBool::new(false),
            folded: AtomicU64::new(0),
            painted: AtomicU64::new(0),
            seq: AtomicU64::new(0),
            traits,
            journal: Mutex::new(Journal::default()),
            route: Mutex::new(None),
            tick_fires: Mutex::new(BTreeMap::new()),
        }
    }

    /// Count one fire of a declared tick. Ticks are counted, not journaled: a frame-rate tick
    /// would otherwise evict every user-driven trace from the bounded journal.
    pub fn note_tick_fired(&self, trait_name: &str, tick_name: &str) {
        *self
            .tick_fires
            .lock()
            .entry((trait_name.to_string(), tick_name.to_string()))
            .or_insert(0) += 1;
    }

    pub fn tick_fires(&self) -> Vec<Value> {
        self.tick_fires
            .lock()
            .iter()
            .map(|((trait_name, tick), fires)| {
                serde_json::json!({ "trait": trait_name, "tick": tick, "fires": fires })
            })
            .collect()
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    pub fn traits(&self) -> &[TraitMeta] {
        &self.traits
    }

    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }

    pub fn queued_sends(&self) -> usize {
        self.queued_sends.load(Ordering::SeqCst)
    }

    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::SeqCst);
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    pub fn set_folded(&self, generation: u64) {
        self.folded.fetch_max(generation, Ordering::SeqCst);
    }

    pub fn set_painted(&self, generation: u64) {
        self.painted.fetch_max(generation, Ordering::SeqCst);
    }

    /// The host folded a render it has not drawn yet.
    pub fn unpainted(&self) -> bool {
        self.painted.load(Ordering::SeqCst) < self.folded.load(Ordering::SeqCst)
    }

    pub fn idle(&self) -> bool {
        self.is_ready() && self.pending() == 0 && self.queued_sends() == 0 && !self.unpainted()
    }

    pub fn note_send_accepted(&self) {
        self.queued_sends.fetch_add(1, Ordering::SeqCst);
    }

    /// Any host dispatch consumes one accepted `/send` that was still hopping to the main thread.
    pub fn note_dispatch_call(&self) {
        saturating_decrement(&self.queued_sends);
    }

    pub fn note_restore(&self) {
        saturating_decrement(&self.pending);
    }

    /// The abandoned leg's trait has no single owning orbital: lower `pending`, journal nothing.
    pub fn note_server_leg_abandoned_unattributed(&self) {
        saturating_decrement(&self.pending);
    }

    /// A server leg that failed at transport level with no snapshot to restore: lower `pending` and journal why.
    pub fn note_server_leg_abandoned(
        &self,
        orbital: &str,
        event: Option<&str>,
        error: Option<&str>,
    ) {
        saturating_decrement(&self.pending);
        if !self.is_enabled() {
            return;
        }
        self.journal.lock().push_event(EventLogEntry {
            event_type: "server-leg-failed".to_string(),
            payload: Some(
                serde_json::json!({ "orbital": orbital, "event": event, "error": error }),
            ),
            source: None,
            timestamp: now_ms(),
        });
    }

    pub fn set_route(&self, route: Option<String>) {
        *self.route.lock() = route;
    }

    pub fn route(&self) -> Option<String> {
        self.route.lock().clone()
    }

    fn next_trace_id(&self) -> String {
        format!("t-{}", self.seq.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// Record one dispatch: the seed event, what it emitted, and one trace per delivery the
    /// cascade executed (a self-loop included), each with the event and payload it ran on.
    /// `server_legs` raises `pending` once per leg the host must post.
    pub fn record_dispatch(
        &self,
        seed_trait: &str,
        event: &str,
        payload: &CoreValue,
        response: &EventResponse,
        outcomes: &[(String, ProcessEventResult)],
        server_legs: usize,
    ) {
        self.record_dispatch_from(
            seed_trait,
            event,
            payload,
            response,
            outcomes,
            server_legs,
            None,
        );
    }

    /// [`record_dispatch`](Self::record_dispatch) for a dispatch a tick's `emit` started: the
    /// event entry's source and every resulting trace carry the tick name.
    #[allow(clippy::too_many_arguments)]
    pub fn record_dispatch_from(
        &self,
        seed_trait: &str,
        event: &str,
        payload: &CoreValue,
        response: &EventResponse,
        outcomes: &[(String, ProcessEventResult)],
        server_legs: usize,
        tick: Option<&str>,
    ) {
        self.pending.fetch_add(server_legs, Ordering::SeqCst);
        if !self.is_enabled() {
            return;
        }
        let now = now_ms();
        let mut journal = self.journal.lock();
        journal.push_event(EventLogEntry {
            event_type: event.to_string(),
            payload: (!payload.is_null())
                .then(|| serde_json::to_value(payload).ok())
                .flatten(),
            source: Some(match tick {
                Some(tick) => serde_json::json!({ "trait": seed_trait, "tick": tick }),
                None => serde_json::json!({ "trait": seed_trait }),
            }),
            timestamp: now,
        });
        for emitted in &response.emitted_events {
            journal.push_event(EventLogEntry {
                event_type: emitted.event.clone(),
                payload: emitted
                    .payload
                    .as_ref()
                    .and_then(|p| serde_json::to_value(p).ok()),
                source: emitted
                    .source
                    .as_ref()
                    .and_then(|s| serde_json::to_value(s).ok()),
                timestamp: now,
            });
        }

        let by_trait = response.client_effects_by_trait.as_deref().unwrap_or(&[]);
        let mut seed_recorded = false;
        for (trait_name, result) in outcomes {
            if !result.executed {
                continue;
            }
            let fired = result.event.clone().unwrap_or_default();
            let is_seed = !seed_recorded && trait_name == seed_trait && fired == event;
            if is_seed {
                seed_recorded = true;
            } else if !fired.is_empty() && !already_on_the_bus(response, &fired) {
                journal.push_event(EventLogEntry {
                    event_type: fired.clone(),
                    payload: (!result.payload.is_null())
                        .then(|| serde_json::to_value(&result.payload).ok())
                        .flatten(),
                    source: Some(match tick {
                        Some(tick) => serde_json::json!({ "trait": trait_name, "tick": tick }),
                        None => serde_json::json!({ "trait": trait_name }),
                    }),
                    timestamp: now,
                });
            }
            let mut effects: Vec<EffectTrace> = by_trait
                .iter()
                .filter(|e| &e.trait_name == trait_name && e.event == result.event)
                .map(|e| client_effect_trace(&e.effect))
                .collect();
            if is_seed {
                effects.extend(
                    response
                        .effect_results
                        .iter()
                        .flatten()
                        .map(server_effect_trace),
                );
            }
            journal.push_transition(TransitionTrace {
                id: self.next_trace_id(),
                trait_name: trait_name.clone(),
                from: result.previous_state.clone(),
                to: result.new_state.clone(),
                event: fired,
                effects,
                server_response: None,
                tick: tick.map(str::to_string),
                timestamp: now,
            });
        }
    }

    /// Record a server response folded into the kernel (the counterpart to a pending server leg).
    pub fn record_applied(
        &self,
        orbital_of: &dyn Fn(&str) -> String,
        response: &EventResponse,
        outcomes: &[(String, ProcessEventResult)],
    ) {
        saturating_decrement(&self.pending);
        if !self.is_enabled() {
            return;
        }
        let now = now_ms();
        let server_for = |trait_name: &str| ServerResponseTrace {
            orbital_name: orbital_of(trait_name),
            success: response.success,
            transitioned: response.transitioned,
            client_effects: response.client_effects.as_ref().map_or(0, Vec::len),
            data_entities: response
                .data
                .iter()
                .flatten()
                .map(|(name, rows)| (name.clone(), rows.len()))
                .collect(),
            emitted_events: response
                .emitted_events
                .iter()
                .map(|e| e.event.clone())
                .collect(),
            error: response.error.clone(),
            timestamp: now,
        };
        let mut journal = self.journal.lock();
        for server_effect in response.effect_results.iter().flatten() {
            journal.push_event(EventLogEntry {
                event_type: SERVER_EFFECT_EVENT.to_string(),
                payload: serde_json::to_value(server_effect_trace(server_effect)).ok(),
                source: Some(serde_json::json!({ "fromServer": true })),
                timestamp: now,
            });
        }
        for emitted in &response.emitted_events {
            journal.push_event(EventLogEntry {
                event_type: emitted.event.clone(),
                payload: emitted
                    .payload
                    .as_ref()
                    .and_then(|p| serde_json::to_value(p).ok()),
                source: emitted
                    .source
                    .as_ref()
                    .and_then(|s| serde_json::to_value(s).ok()),
                timestamp: now,
            });
        }
        for (trait_name, result) in outcomes {
            if !result.executed {
                continue;
            }
            let event = result.event.clone().unwrap_or_default();
            if !event.is_empty() && !already_on_the_bus(response, &event) {
                journal.push_event(EventLogEntry {
                    event_type: event.clone(),
                    payload: (!result.payload.is_null())
                        .then(|| serde_json::to_value(&result.payload).ok())
                        .flatten(),
                    source: Some(serde_json::json!({ "trait": trait_name, "fromServer": true })),
                    timestamp: now,
                });
            }
            journal.push_transition(TransitionTrace {
                id: self.next_trace_id(),
                trait_name: trait_name.clone(),
                from: result.previous_state.clone(),
                to: result.new_state.clone(),
                event,
                effects: Vec::new(),
                server_response: Some(server_for(trait_name)),
                tick: None,
                timestamp: now,
            });
        }
    }

    pub fn transitions(&self) -> (Vec<TransitionTrace>, u64) {
        let journal = self.journal.lock();
        (
            journal.transitions.iter().cloned().collect(),
            journal.transitions_dropped,
        )
    }

    /// Entries at absolute position >= `since`. A `since` below `dropped` starts at the oldest kept entry.
    pub fn events_since(&self, since: u64) -> EventsPage {
        let journal = self.journal.lock();
        let dropped = journal.events_dropped;
        let skip = since.saturating_sub(dropped) as usize;
        let entries: Vec<EventLogEntry> = journal.events.iter().skip(skip).cloned().collect();
        EventsPage {
            next: dropped + journal.events.len() as u64,
            entries,
            dropped,
        }
    }
}

/// Everything a handle carries for the bridge.
pub struct HandleBridge {
    pub recorder: Arc<Recorder>,
    pub callback: SharedCallback,
    pub server: Mutex<Option<BridgeServer>>,
}

impl HandleBridge {
    pub fn new(traits: Vec<TraitMeta>) -> Self {
        Self {
            recorder: Arc::new(Recorder::new(traits)),
            callback: Arc::new(Mutex::new(None)),
            server: Mutex::new(None),
        }
    }
}

/// The recorder watching an embedded kernel.
pub struct RecorderObserver(pub Arc<Recorder>);

impl orbital_core::embed::EmbedObserver for RecorderObserver {
    fn dispatched(
        &self,
        trait_name: &str,
        event: &str,
        payload: &CoreValue,
        dispatch: &orbital_core::runtime::ClientDispatch,
        tick: Option<&str>,
    ) {
        self.0.record_dispatch_from(
            trait_name,
            event,
            payload,
            &dispatch.response,
            &dispatch.outcomes,
            dispatch.server_legs.len(),
            tick,
        );
    }

    fn applied(
        &self,
        owners: &orbital_core::runtime::TraitOwners,
        response: &EventResponse,
        outcomes: &[(String, ProcessEventResult)],
    ) {
        self.0.record_applied(
            &|trait_name: &str| owners.label(trait_name),
            response,
            outcomes,
        );
    }

    fn restored(&self) {
        self.0.note_restore();
    }

    fn tick_fired(&self, trait_name: &str, tick: &str) {
        self.0.note_tick_fired(trait_name, tick);
    }
}

/// The `entity_id` `dispatch_with_server_leg` defaults to, so state reads address the same instance.
const SINGLETON: &str = "singleton";

/// A trait's current state — its declared initial one while untouched; `None` for an unknown trait.
fn current_state(kernel: &RuntimeKernel, trait_name: &str) -> Option<String> {
    kernel
        .trait_snapshot(Some(SINGLETON))
        .remove(trait_name)
        .map(|(state, _)| state)
        .filter(|s| !s.is_empty())
}

fn trait_states(kernel: &RuntimeKernel, traits: &[TraitMeta]) -> HashMap<String, String> {
    let mut all = kernel.trait_snapshot(Some(SINGLETON));
    traits
        .iter()
        .map(|t| {
            let state = all
                .remove(&t.trait_name)
                .map(|(state, _)| state)
                .unwrap_or_default();
            (t.trait_name.clone(), state)
        })
        .collect()
}

#[derive(Clone, Copy)]
struct KernelPtr(*const RuntimeKernel);
// SAFETY: the pointee is the kernel inside the boxed handle; `BridgeServer::stop`
// joins the thread before `orbital_kernel_destroy` drops the box.
unsafe impl Send for KernelPtr {}
unsafe impl Sync for KernelPtr {}

struct ServerCtx {
    kernel: KernelPtr,
    entities: Arc<InMemoryEntityCache>,
    recorder: Arc<Recorder>,
    callback: SharedCallback,
    call_lock: Arc<RwLock<()>>,
}

pub struct BridgeServer {
    port: u16,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl BridgeServer {
    /// Bind 127.0.0.1:`port` (0 = ephemeral) and serve on one background thread.
    ///
    /// # Safety
    /// `kernel` must outlive the server: callers `stop()` before dropping it.
    pub unsafe fn start(
        kernel: *const RuntimeKernel,
        entities: Arc<InMemoryEntityCache>,
        recorder: Arc<Recorder>,
        callback: SharedCallback,
        call_lock: Arc<RwLock<()>>,
        port: u16,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        listener.set_nonblocking(true)?;
        let bound = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = Arc::new(ServerCtx {
            kernel: KernelPtr(kernel),
            entities,
            recorder,
            callback,
            call_lock,
        });
        let thread_stop = Arc::clone(&stop);
        let join = std::thread::Builder::new()
            .name("orbital-verify-bridge".into())
            .spawn(move || {
                while !thread_stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let ctx = Arc::clone(&ctx);
                            let _ =
                                catch_unwind(AssertUnwindSafe(|| serve_connection(stream, &ctx)));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(ACCEPT_POLL)
                        }
                        Err(_) => std::thread::sleep(ACCEPT_POLL),
                    }
                }
            })?;
        Ok(Self {
            port: bound,
            stop,
            join: Some(join),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for BridgeServer {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    body: Vec<u8>,
}

enum ParseError {
    Status(u16, &'static str),
    Closed,
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(raw: &str) -> HashMap<String, String> {
    raw.split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn read_request(stream: &mut TcpStream) -> Result<Request, ParseError> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(end) = find_header_end(&buf) {
            break end;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err(ParseError::Status(431, "request headers exceed 64 KiB"));
        }
        let n = stream.read(&mut chunk).map_err(|_| ParseError::Closed)?;
        if n == 0 {
            return Err(ParseError::Closed);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..header_end])
        .map_err(|_| ParseError::Status(400, "headers are not utf-8"))?;
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or(ParseError::Status(400, "empty request"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or(ParseError::Status(400, "no method"))?
        .to_string();
    let target = parts.next().ok_or(ParseError::Status(400, "no target"))?;
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (target.to_string(), HashMap::new()),
    };
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value
                    .trim()
                    .parse()
                    .map_err(|_| ParseError::Status(400, "bad content-length"))?;
            }
        }
    }
    if content_length > MAX_BODY_BYTES {
        return Err(ParseError::Status(413, "request body exceeds 1 MiB"));
    }
    let mut body: Vec<u8> = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).map_err(|_| ParseError::Closed)?;
        if n == 0 {
            return Err(ParseError::Status(400, "body shorter than content-length"));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    Ok(Request {
        method,
        path,
        query,
        body,
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    }
}

fn write_response(stream: &mut TcpStream, status: u16, body: &Value) {
    let payload = body.to_string();
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason(status),
        payload.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(payload.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
}

fn error_body(message: &str) -> Value {
    serde_json::json!({ "error": message })
}

fn serve_connection(mut stream: TcpStream, ctx: &ServerCtx) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
    let (status, body) = match read_request(&mut stream) {
        Ok(req) => route(&req, ctx),
        Err(ParseError::Status(code, msg)) => (code, error_body(msg)),
        Err(ParseError::Closed) => return,
    };
    write_response(&mut stream, status, &body);
}

fn kernel_of(ctx: &ServerCtx) -> &RuntimeKernel {
    // SAFETY: see `KernelPtr`.
    unsafe { &*ctx.kernel.0 }
}

fn route(req: &Request, ctx: &ServerCtx) -> (u16, Value) {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/health") => (200, serde_json::json!({ "ok": true })),
        ("GET", "/snapshot") => {
            let kernel = kernel_of(ctx);
            let _call = ctx.call_lock.read();
            let states = trait_states(kernel, ctx.recorder.traits());
            let traits: Vec<Value> = ctx
                .recorder
                .traits()
                .iter()
                .map(|t| {
                    let current = states.get(&t.trait_name).cloned().unwrap_or_default();
                    serde_json::json!({
                        "orbital": t.orbital,
                        "traitName": t.trait_name,
                        "currentState": current,
                        "states": t.states,
                        "events": t.events,
                    })
                })
                .collect();
            let (transitions, dropped) = ctx.recorder.transitions();
            (
                200,
                serde_json::json!({ "traits": traits, "transitions": transitions, "transitionsDropped": dropped, "route": ctx.recorder.route(), "tickFires": ctx.recorder.tick_fires() }),
            )
        }
        ("GET", "/route") => (200, serde_json::json!({ "route": ctx.recorder.route() })),
        ("GET", "/state") => match req.query.get("trait") {
            Some(name) => {
                let _call = ctx.call_lock.read();
                let state = current_state(kernel_of(ctx), name);
                (200, serde_json::json!({ "state": state }))
            }
            None => (400, error_body("missing query parameter: trait")),
        },
        ("GET", "/events") => {
            let since = match req.query.get("since") {
                Some(raw) => match raw.parse::<u64>() {
                    Ok(n) => n,
                    Err(_) => return (400, error_body("since must be a non-negative integer")),
                },
                None => 0,
            };
            (
                200,
                serde_json::to_value(ctx.recorder.events_since(since)).unwrap_or(Value::Null),
            )
        }
        ("GET", "/entities") => match req.query.get("name") {
            Some(name) => match ctx.entities.list(name) {
                Ok(rows) => {
                    let rows: Vec<&Value> = rows.iter().map(|row| &**row).collect();
                    (200, serde_json::json!({ "rows": rows }))
                }
                Err(e) => (400, error_body(&e)),
            },
            None => (400, error_body("missing query parameter: name")),
        },
        ("GET", "/idle") => (
            200,
            serde_json::json!({
                "idle": ctx.recorder.idle(),
                "ready": ctx.recorder.is_ready(),
                "pending": ctx.recorder.pending(),
                "queuedSends": ctx.recorder.queued_sends(),
                "unpainted": ctx.recorder.unpainted(),
            }),
        ),
        ("POST", "/send") => send(req, ctx),
        (
            _,
            "/health" | "/snapshot" | "/state" | "/events" | "/entities" | "/idle" | "/route"
            | "/send",
        ) => (405, error_body("method not allowed")),
        _ => (404, error_body("not found")),
    }
}

fn send(req: &Request, ctx: &ServerCtx) -> (u16, Value) {
    let body: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(e) => return (400, error_body(&format!("body is not json: {e}"))),
    };
    let Some(event) = body
        .get("event")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return (400, error_body("body.event must be a non-empty string"));
    };
    let trait_scope = match body.get("traitScope") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.as_str()),
        Some(_) => return (400, error_body("body.traitScope must be a string")),
    };
    let payload = match body.get("payload") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.to_string()),
    };
    let Some(slot) = *ctx.callback.lock() else {
        return (
            409,
            error_body("no dispatch callback registered by the host"),
        );
    };
    let (Ok(c_event), Ok(c_scope), Ok(c_payload)) = (
        CString::new(event),
        trait_scope.map(CString::new).transpose(),
        payload.map(CString::new).transpose(),
    ) else {
        return (400, error_body("send fields must not contain NUL"));
    };
    ctx.recorder.note_send_accepted();
    (slot.callback)(
        slot.ctx as *mut c_void,
        c_scope.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
        c_event.as_ptr(),
        c_payload.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
    );
    (202, serde_json::json!({ "accepted": true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel_c_api::*;
    use std::ffi::{c_char, CStr};

    fn cstr(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn schema_json() -> String {
        crate::resolved_fixture("basic-entity")
    }

    unsafe fn new_handle() -> *mut OrbitalKernelHandle {
        let schema = cstr(&schema_json());
        let client = cstr("verify-test");
        let handle = orbital_kernel_init(schema.as_ptr(), client.as_ptr());
        assert!(!handle.is_null());
        handle
    }

    unsafe fn started_unready() -> (*mut OrbitalKernelHandle, u16) {
        let handle = new_handle();
        let port = orbital_verify_bridge_start(handle, 0);
        assert!(port > 0, "bridge failed to start");
        (handle, port as u16)
    }

    unsafe fn started() -> (*mut OrbitalKernelHandle, u16) {
        let (handle, port) = started_unready();
        assert_eq!(orbital_verify_bridge_set_ready(handle, 1), 1);
        (handle, port)
    }

    unsafe fn dispatch(handle: *mut OrbitalKernelHandle, trait_name: &str, event: &str) -> Value {
        let t = cstr(trait_name);
        let e = cstr(event);
        let out = orbital_kernel_dispatch(
            handle,
            t.as_ptr(),
            std::ptr::null(),
            e.as_ptr(),
            std::ptr::null(),
        );
        assert!(!out.is_null());
        let wire: Value = serde_json::from_str(CStr::from_ptr(out).to_str().unwrap()).unwrap();
        orbital_kernel_free_string(out);
        wire
    }

    fn http(port: u16, method: &str, target: &str, body: Option<&str>) -> (u16, Value) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let body = body.unwrap_or("");
        let req = format!(
            "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(req.as_bytes()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let status: u16 = raw.split_whitespace().nth(1).unwrap().parse().unwrap();
        let payload = raw.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
        (status, serde_json::from_str(payload).unwrap_or(Value::Null))
    }

    fn http_in_background(
        port: u16,
        target: &'static str,
    ) -> (
        std::sync::mpsc::Receiver<(u16, Value)>,
        std::thread::JoinHandle<()>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let join = std::thread::spawn(move || {
            let _ = tx.send(http(port, "GET", target, None));
        });
        (rx, join)
    }

    #[test]
    fn bridge_state_reads_wait_for_an_in_flight_kernel_call() {
        unsafe {
            let (handle, port) = started();
            let held = (*handle).call_lock.write();
            assert_eq!(http(port, "GET", "/health", None).0, 200);
            let (snapshot, snapshot_join) = http_in_background(port, "/snapshot");
            assert!(
                snapshot
                    .recv_timeout(std::time::Duration::from_millis(300))
                    .is_err(),
                "/snapshot read the kernel while a call held the lock"
            );
            drop(held);
            let (status, _) = snapshot
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("/snapshot completes once the lock is free");
            assert_eq!(status, 200);
            snapshot_join.join().unwrap();

            let held = (*handle).call_lock.write();
            let (state, state_join) = http_in_background(port, "/state?trait=TaskCRUD");
            assert!(
                state
                    .recv_timeout(std::time::Duration::from_millis(300))
                    .is_err(),
                "/state read the kernel while a call held the lock"
            );
            drop(held);
            let (status, body) = state
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("/state completes once the lock is free");
            assert_eq!((status, body["state"].as_str()), (200, Some("listing")));
            state_join.join().unwrap();
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn bridge_state_reads_do_not_wait_when_no_kernel_call_is_in_flight() {
        unsafe {
            let (handle, port) = started();
            let (state, join) = http_in_background(port, "/state?trait=TaskCRUD");
            let (status, _) = state
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("/state is prompt with the lock free");
            assert_eq!(status, 200);
            join.join().unwrap();
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn start_serves_health_and_a_second_start_returns_the_same_port() {
        unsafe {
            let (handle, port) = started();
            assert_eq!(
                http(port, "GET", "/health", None),
                (200, serde_json::json!({ "ok": true }))
            );
            assert_eq!(orbital_verify_bridge_start(handle, 0), i32::from(port));
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn destroy_stops_the_server() {
        unsafe {
            let (handle, port) = started();
            orbital_kernel_destroy(handle);
            assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
        }
    }

    #[test]
    fn stop_closes_the_port_and_a_restart_serves_again() {
        unsafe {
            let (handle, port) = started();
            orbital_verify_bridge_stop(handle);
            assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
            let again = orbital_verify_bridge_start(handle, 0);
            assert!(again > 0);
            assert_eq!(http(again as u16, "GET", "/health", None).0, 200);
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn start_on_a_taken_port_returns_zero_and_sets_last_error() {
        unsafe {
            let (first, port) = started();
            let second = new_handle();
            assert_eq!(orbital_verify_bridge_start(second, port), 0);
            let err = orbital_kernel_last_error(second);
            assert!(CStr::from_ptr(err).to_str().unwrap().contains("bind"));
            orbital_kernel_free_string(err);
            orbital_kernel_destroy(second);
            orbital_kernel_destroy(first);
        }
    }

    #[test]
    fn start_with_a_null_handle_returns_zero() {
        unsafe {
            assert_eq!(orbital_verify_bridge_start(std::ptr::null_mut(), 0), 0);
        }
    }

    #[test]
    fn snapshot_lists_declared_traits_states_and_events() {
        unsafe {
            let (handle, port) = started();
            let (status, body) = http(port, "GET", "/snapshot", None);
            assert_eq!(status, 200);
            let task = &body["traits"][0];
            assert_eq!(task["traitName"], "TaskCRUD");
            assert_eq!(task["currentState"], "listing");
            assert_eq!(
                task["states"],
                serde_json::json!(["listing", "creating", "editing"])
            );
            let events: Vec<&str> = task["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e.as_str().unwrap())
                .collect();
            for expected in ["INIT", "CREATE", "SAVE", "CANCEL"] {
                assert!(
                    events.contains(&expected),
                    "missing {expected} in {events:?}"
                );
            }
            assert_eq!(body["transitions"], serde_json::json!([]));
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn dispatch_records_a_transition_trace_and_an_event_log_entry() {
        unsafe {
            let (handle, port) = started();
            dispatch(handle, "TaskCRUD", "CREATE");
            let (_, snap) = http(port, "GET", "/snapshot", None);
            let trace = &snap["transitions"][0];
            assert_eq!(trace["traitName"], "TaskCRUD");
            assert_eq!(trace["from"], "listing");
            assert_eq!(trace["to"], "creating");
            assert_eq!(trace["event"], "CREATE");
            assert!(trace["id"].as_str().unwrap().starts_with("t-"));
            assert!(trace["timestamp"].as_u64().unwrap() > 0);
            assert!(trace["effects"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["type"] == "render-ui"));
            let (_, state) = http(port, "GET", "/state?trait=TaskCRUD", None);
            assert_eq!(state["state"], "creating");
            let (_, events) = http(port, "GET", "/events?since=0", None);
            assert_eq!(events["entries"][0]["type"], "CREATE");
            assert_eq!(events["dropped"], 0);
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn a_self_loop_dispatch_still_records_a_trace() {
        unsafe {
            let (handle, port) = started();
            dispatch(handle, "TaskCRUD", "INIT");
            let (_, snap) = http(port, "GET", "/snapshot", None);
            let trace = &snap["transitions"][0];
            assert_eq!(
                (trace["from"].as_str(), trace["to"].as_str()),
                (Some("listing"), Some("listing"))
            );
            assert_eq!(trace["event"], "INIT");
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn a_dispatch_with_no_matching_arm_records_no_trace() {
        unsafe {
            let (handle, port) = started();
            dispatch(handle, "TaskCRUD", "CANCEL");
            let (_, snap) = http(port, "GET", "/snapshot", None);
            assert_eq!(snap["transitions"], serde_json::json!([]));
            let (_, events) = http(port, "GET", "/events", None);
            assert_eq!(events["entries"][0]["type"], "CANCEL");
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn dispatch_before_start_is_not_recorded() {
        unsafe {
            let handle = new_handle();
            dispatch(handle, "TaskCRUD", "CREATE");
            let port = orbital_verify_bridge_start(handle, 0) as u16;
            let (_, snap) = http(port, "GET", "/snapshot", None);
            assert_eq!(snap["transitions"], serde_json::json!([]));
            assert_eq!(snap["traits"][0]["currentState"], "creating");
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn events_cursor_advances_and_a_caught_up_reader_gets_nothing() {
        unsafe {
            let (handle, port) = started();
            dispatch(handle, "TaskCRUD", "CREATE");
            let (_, first) = http(port, "GET", "/events?since=0", None);
            let next = first["next"].as_u64().unwrap();
            assert!(next >= 1);
            let (_, caught_up) = http(port, "GET", &format!("/events?since={next}"), None);
            assert_eq!(caught_up["entries"], serde_json::json!([]));
            assert_eq!(caught_up["next"].as_u64().unwrap(), next);
            dispatch(handle, "TaskCRUD", "CANCEL");
            let (_, later) = http(port, "GET", &format!("/events?since={next}"), None);
            assert_eq!(later["entries"][0]["type"], "CANCEL");
            assert_eq!(http(port, "GET", "/events?since=abc", None).0, 400);
            orbital_kernel_destroy(handle);
        }
    }

    fn quiet_response() -> EventResponse {
        EventResponse::default()
    }

    #[test]
    fn event_log_is_bounded_with_an_explicit_dropped_count() {
        let recorder = Recorder::new(Vec::new());
        recorder.set_enabled(true);
        let extra = 3;
        for i in 0..EVENT_LOG_CAPACITY + extra {
            recorder.record_dispatch(
                "T",
                &format!("E{i}"),
                &CoreValue::Null,
                &quiet_response(),
                &[],
                0,
            );
        }
        let page = recorder.events_since(0);
        assert_eq!(page.dropped, extra as u64);
        assert_eq!(page.entries.len(), EVENT_LOG_CAPACITY);
        assert_eq!(page.entries[0].event_type, format!("E{extra}"));
        assert_eq!(page.next, (EVENT_LOG_CAPACITY + extra) as u64);
        let tail = recorder.events_since(page.next - 1);
        assert_eq!(tail.entries.len(), 1);
        assert_eq!(
            tail.entries[0].event_type,
            format!("E{}", EVENT_LOG_CAPACITY + extra - 1)
        );
    }

    #[test]
    fn event_log_under_capacity_drops_nothing() {
        let recorder = Recorder::new(Vec::new());
        recorder.set_enabled(true);
        recorder.record_dispatch("T", "E", &CoreValue::Null, &quiet_response(), &[], 0);
        let page = recorder.events_since(0);
        assert_eq!((page.dropped, page.entries.len(), page.next), (0, 1, 1));
    }

    #[test]
    fn a_disabled_recorder_journals_nothing_but_still_counts_pending() {
        let recorder = Recorder::new(Vec::new());
        recorder.record_dispatch("T", "E", &CoreValue::Null, &quiet_response(), &[], 1);
        assert_eq!(recorder.events_since(0).entries.len(), 0);
        assert_eq!(recorder.pending(), 1);
    }

    #[test]
    fn send_without_a_callback_is_409() {
        unsafe {
            let (handle, port) = started();
            let (status, body) = http(port, "POST", "/send", Some(r#"{"event":"CREATE"}"#));
            assert_eq!(status, 409);
            assert!(body["error"].as_str().unwrap().contains("callback"));
            orbital_kernel_destroy(handle);
        }
    }

    type Captured = Mutex<Vec<(Option<String>, String, Option<String>)>>;

    extern "C" fn capture(
        ctx: *mut c_void,
        scope: *const c_char,
        event: *const c_char,
        payload: *const c_char,
    ) {
        let captured = unsafe { &*(ctx as *const Captured) };
        let read = |p: *const c_char| {
            (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string())
        };
        captured
            .lock()
            .push((read(scope), read(event).unwrap(), read(payload)));
    }

    #[test]
    fn send_with_a_callback_delivers_scope_event_and_payload_and_returns_202() {
        unsafe {
            let (handle, port) = started();
            let captured: Captured = Mutex::new(Vec::new());
            assert_eq!(
                orbital_verify_bridge_set_dispatch_callback(
                    handle,
                    Some(capture),
                    &captured as *const Captured as *mut c_void
                ),
                1
            );
            let (status, _) = http(
                port,
                "POST",
                "/send",
                Some(r#"{"event":"SAVE","traitScope":"TaskCRUD","payload":{"id":"7"}}"#),
            );
            assert_eq!(status, 202);
            let (status, _) = http(port, "POST", "/send", Some(r#"{"event":"CREATE"}"#));
            assert_eq!(status, 202);
            let calls = captured.lock().clone();
            assert_eq!(
                calls,
                vec![
                    (
                        Some("TaskCRUD".to_string()),
                        "SAVE".to_string(),
                        Some(r#"{"id":"7"}"#.to_string())
                    ),
                    (None, "CREATE".to_string(), None),
                ]
            );
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn send_rejects_malformed_bodies_without_calling_the_callback() {
        unsafe {
            let (handle, port) = started();
            let captured: Captured = Mutex::new(Vec::new());
            orbital_verify_bridge_set_dispatch_callback(
                handle,
                Some(capture),
                &captured as *const Captured as *mut c_void,
            );
            assert_eq!(http(port, "POST", "/send", Some("not json")).0, 400);
            assert_eq!(
                http(port, "POST", "/send", Some(r#"{"payload":{}}"#)).0,
                400
            );
            assert_eq!(http(port, "POST", "/send", Some(r#"{"event":""}"#)).0, 400);
            assert_eq!(
                http(
                    port,
                    "POST",
                    "/send",
                    Some(r#"{"event":"X","traitScope":3}"#)
                )
                .0,
                400
            );
            assert!(captured.lock().is_empty());
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn clearing_the_callback_returns_send_to_409() {
        unsafe {
            let (handle, port) = started();
            let captured: Captured = Mutex::new(Vec::new());
            orbital_verify_bridge_set_dispatch_callback(
                handle,
                Some(capture),
                &captured as *const Captured as *mut c_void,
            );
            orbital_verify_bridge_set_dispatch_callback(handle, None, std::ptr::null_mut());
            assert_eq!(
                http(port, "POST", "/send", Some(r#"{"event":"CREATE"}"#)).0,
                409
            );
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn an_accepted_send_keeps_the_bridge_busy_until_the_host_dispatches() {
        unsafe {
            let (handle, port) = started();
            let captured: Captured = Mutex::new(Vec::new());
            orbital_verify_bridge_set_dispatch_callback(
                handle,
                Some(capture),
                &captured as *const Captured as *mut c_void,
            );
            assert_eq!(http(port, "GET", "/idle", None).1["idle"], true);
            http(port, "POST", "/send", Some(r#"{"event":"CANCEL"}"#));
            let (_, busy) = http(port, "GET", "/idle", None);
            assert_eq!(
                (busy["idle"].clone(), busy["queuedSends"].clone()),
                (Value::Bool(false), serde_json::json!(1))
            );
            dispatch(handle, "TaskCRUD", "CANCEL");
            assert_eq!(http(port, "GET", "/idle", None).1["idle"], true);
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn idle_is_false_before_ready_even_with_nothing_pending() {
        unsafe {
            let (handle, port) = started_unready();
            let (_, body) = http(port, "GET", "/idle", None);
            assert_eq!(
                (
                    body["idle"].clone(),
                    body["ready"].clone(),
                    body["pending"].clone(),
                    body["queuedSends"].clone()
                ),
                (
                    Value::Bool(false),
                    Value::Bool(false),
                    serde_json::json!(0),
                    serde_json::json!(0)
                )
            );
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn idle_is_true_once_ready_and_false_again_when_ready_is_withdrawn() {
        unsafe {
            let (handle, port) = started_unready();
            assert_eq!(orbital_verify_bridge_set_ready(handle, 1), 1);
            let (_, body) = http(port, "GET", "/idle", None);
            assert_eq!(
                (body["idle"].clone(), body["ready"].clone()),
                (Value::Bool(true), Value::Bool(true))
            );
            assert_eq!(orbital_verify_bridge_set_ready(handle, 0), 1);
            let (_, body) = http(port, "GET", "/idle", None);
            assert_eq!(
                (body["idle"].clone(), body["ready"].clone()),
                (Value::Bool(false), Value::Bool(false))
            );
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn idle_waits_until_the_host_has_painted_what_it_folded() {
        unsafe {
            let (handle, port) = started_unready();
            assert_eq!(orbital_verify_bridge_set_ready(handle, 1), 1);
            assert_eq!(orbital_verify_bridge_folded(handle, 3), 1);
            let (_, body) = http(port, "GET", "/idle", None);
            assert_eq!(
                (body["idle"].clone(), body["unpainted"].clone()),
                (Value::Bool(false), Value::Bool(true)),
                "the kernel moved on, the screen has not"
            );
            assert_eq!(orbital_verify_bridge_painted(handle, 2), 1);
            assert_eq!(
                http(port, "GET", "/idle", None).1["idle"],
                false,
                "an older paint"
            );
            assert_eq!(orbital_verify_bridge_painted(handle, 3), 1);
            let (_, body) = http(port, "GET", "/idle", None);
            assert_eq!(
                (body["idle"].clone(), body["unpainted"].clone()),
                (Value::Bool(true), Value::Bool(false))
            );
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn control_a_host_that_reports_no_fold_is_idle_as_before() {
        unsafe {
            let (handle, port) = started_unready();
            assert_eq!(orbital_verify_bridge_set_ready(handle, 1), 1);
            assert_eq!(http(port, "GET", "/idle", None).1["idle"], true);
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn ready_does_not_mask_a_pending_server_leg() {
        unsafe {
            let (handle, port) = started();
            dispatch(handle, "TaskCRUD", "INIT");
            let (_, body) = http(port, "GET", "/idle", None);
            assert_eq!(
                (
                    body["ready"].clone(),
                    body["idle"].clone(),
                    body["pending"].clone()
                ),
                (Value::Bool(true), Value::Bool(false), serde_json::json!(1))
            );
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn a_leg_issued_while_not_ready_is_still_counted() {
        unsafe {
            let (handle, port) = started_unready();
            dispatch(handle, "TaskCRUD", "INIT");
            orbital_verify_bridge_set_ready(handle, 1);
            let (_, body) = http(port, "GET", "/idle", None);
            assert_eq!(
                (body["idle"].clone(), body["pending"].clone()),
                (Value::Bool(false), serde_json::json!(1))
            );
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn a_server_leg_raises_pending_and_apply_server_response_lowers_it() {
        unsafe {
            let (handle, port) = started();
            let wire = dispatch(handle, "TaskCRUD", "INIT");
            assert!(
                !wire["serverLegs"].as_array().unwrap().is_empty(),
                "INIT must carry a server leg: {wire}"
            );
            let (_, busy) = http(port, "GET", "/idle", None);
            assert_eq!(
                (busy["idle"].clone(), busy["pending"].clone()),
                (Value::Bool(false), serde_json::json!(1))
            );

            let response =
                cstr(r#"{"success":true,"transitioned":false,"states":{},"emittedEvents":[]}"#);
            let already = cstr("[]");
            let out =
                orbital_kernel_apply_server_response(handle, response.as_ptr(), already.as_ptr());
            orbital_kernel_free_string(out);
            let (_, idle) = http(port, "GET", "/idle", None);
            assert_eq!(
                (idle["idle"].clone(), idle["pending"].clone()),
                (Value::Bool(true), serde_json::json!(0))
            );

            let out =
                orbital_kernel_apply_server_response(handle, response.as_ptr(), already.as_ptr());
            orbital_kernel_free_string(out);
            assert_eq!(http(port, "GET", "/idle", None).1["pending"], 0);
            orbital_kernel_destroy(handle);
        }
    }

    unsafe fn abandon(
        handle: *mut OrbitalKernelHandle,
        trait_name: &str,
        event: &str,
        error: &str,
    ) {
        let trait_name = cstr(trait_name);
        let event = cstr(event);
        let error = cstr(error);
        orbital_kernel_abandon_server_leg(
            handle,
            trait_name.as_ptr(),
            event.as_ptr(),
            error.as_ptr(),
        );
    }

    #[test]
    fn abandon_server_leg_lowers_pending_and_journals_orbital_event_error() {
        unsafe {
            let (handle, port) = started();
            dispatch(handle, "TaskCRUD", "INIT");
            let (_, busy) = http(port, "GET", "/idle", None);
            assert_eq!(
                (busy["idle"].clone(), busy["pending"].clone()),
                (Value::Bool(false), serde_json::json!(1))
            );
            abandon(handle, "TaskCRUD", "INIT", "Could not connect");
            let (_, idle) = http(port, "GET", "/idle", None);
            assert_eq!(
                (idle["idle"].clone(), idle["pending"].clone()),
                (Value::Bool(true), serde_json::json!(0))
            );
            let (_, events) = http(port, "GET", "/events?since=0", None);
            let failed: Vec<&Value> = events["entries"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["type"] == "server-leg-failed")
                .collect();
            assert_eq!(failed.len(), 1);
            assert_eq!(
                failed[0]["payload"],
                serde_json::json!({
                    "orbital": "Task Management",
                    "event": "INIT",
                    "error": "Could not connect"
                })
            );
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn abandon_server_leg_saturates_at_zero_and_a_null_reason_is_null() {
        unsafe {
            let (handle, port) = started();
            let trait_name = cstr("TaskCRUD");
            orbital_kernel_abandon_server_leg(
                handle,
                trait_name.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
            );
            assert_eq!(http(port, "GET", "/idle", None).1["pending"], 0);
            let (_, events) = http(port, "GET", "/events?since=0", None);
            assert!(events["entries"][0]["payload"]["event"].is_null());
            assert!(events["entries"][0]["payload"]["error"].is_null());
            orbital_kernel_abandon_server_leg(
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
            );
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn abandon_server_leg_labels_the_second_orbital_and_refuses_unattributable_traits() {
        use crate::kernel_c_api::tests::{two_orbital_schema_json, SECOND_ORBITAL};
        unsafe {
            let schema = cstr(&two_orbital_schema_json("ChoreCRUD"));
            let client = cstr("verify-test");
            let handle = orbital_kernel_init(schema.as_ptr(), client.as_ptr());
            let port = orbital_verify_bridge_start(handle, 0) as u16;
            dispatch(handle, "ChoreCRUD", "INIT");
            abandon(handle, "ChoreCRUD", "INIT", "boom");
            let (_, events) = http(port, "GET", "/events?since=0", None);
            let failed: Vec<&Value> = events["entries"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["type"] == "server-leg-failed")
                .collect();
            assert_eq!(
                failed[0]["payload"],
                serde_json::json!({ "orbital": SECOND_ORBITAL, "event": "INIT", "error": "boom" })
            );

            // Control: an unknown trait lowers pending but journals nothing and sets last_error.
            dispatch(handle, "ChoreCRUD", "INIT");
            assert_eq!(http(port, "GET", "/idle", None).1["pending"], 1);
            abandon(handle, "NoSuchTrait", "INIT", "boom");
            assert_eq!(http(port, "GET", "/idle", None).1["pending"], 0);
            let (_, events) = http(port, "GET", "/events?since=0", None);
            let count = events["entries"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["type"] == "server-leg-failed")
                .count();
            assert_eq!(count, 1);
            let err = orbital_kernel_last_error(handle);
            assert_eq!(
                CStr::from_ptr(err).to_str().unwrap(),
                "unknown trait: NoSuchTrait"
            );
            orbital_kernel_free_string(err);
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn snapshot_route_lists_trait_metas_of_every_orbital() {
        use crate::kernel_c_api::tests::{two_orbital_schema_json, FIRST_ORBITAL, SECOND_ORBITAL};
        unsafe {
            let schema = cstr(&two_orbital_schema_json("ChoreCRUD"));
            let client = cstr("verify-test");
            let handle = orbital_kernel_init(schema.as_ptr(), client.as_ptr());
            let port = orbital_verify_bridge_start(handle, 0) as u16;
            let (_, snap) = http(port, "GET", "/snapshot", None);
            let owners: Vec<(&str, &str)> = snap["traits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| {
                    (
                        t["traitName"].as_str().unwrap(),
                        t["orbital"].as_str().unwrap(),
                    )
                })
                .collect();
            assert!(owners.contains(&("TaskCRUD", FIRST_ORBITAL)), "{owners:?}");
            assert!(
                owners.contains(&("ChoreCRUD", SECOND_ORBITAL)),
                "{owners:?}"
            );
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn applied_trace_is_labelled_per_trait_owner() {
        let recorder = Recorder::new(Vec::new());
        recorder.set_enabled(true);
        let result = |new_state: &str| ProcessEventResult {
            executed: true,
            previous_state: "a".into(),
            new_state: new_state.into(),
            effects_executed: 0,
            effect_results: Vec::new(),
            entity_data: CoreValue::Null,
            linked_entity: "Task".into(),
            event: Some("E".into()),
            payload: CoreValue::Null,
            entry: None,
            fired_arm: None,
            guards: Vec::new(),
            effect_runs: Vec::new(),
            cascade_dropped: Vec::new(),
        };
        recorder.record_applied(
            &|t: &str| {
                if t == "A" {
                    "OrbA".to_string()
                } else {
                    "OrbB".to_string()
                }
            },
            &quiet_response(),
            &[
                ("A".to_string(), result("x")),
                ("B".to_string(), result("y")),
            ],
        );
        let (traces, _) = recorder.transitions();
        let labels: Vec<&str> = traces
            .iter()
            .map(|t| t.server_response.as_ref().unwrap().orbital_name.as_str())
            .collect();
        assert_eq!(labels, ["OrbA", "OrbB"]);
    }

    #[test]
    fn without_abandon_a_failed_server_leg_stays_pending() {
        unsafe {
            let (handle, port) = started();
            dispatch(handle, "TaskCRUD", "INIT");
            assert_eq!(http(port, "GET", "/idle", None).1["pending"], 1);
            assert_eq!(http(port, "GET", "/idle", None).1["idle"], false);
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn abandon_does_not_lower_a_second_in_flight_leg_below_its_count() {
        unsafe {
            let (handle, port) = started();
            dispatch(handle, "TaskCRUD", "INIT");
            dispatch(handle, "TaskCRUD", "INIT");
            abandon(handle, "TaskCRUD", "INIT", "x");
            assert_eq!(http(port, "GET", "/idle", None).1["pending"], 1);
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn route_is_null_until_the_host_publishes_it_then_served_by_snapshot_and_route() {
        unsafe {
            let (handle, port) = started();
            assert!(http(port, "GET", "/snapshot", None).1["route"].is_null());
            assert!(http(port, "GET", "/route", None).1["route"].is_null());
            let route = cstr("/tasks/42");
            assert_eq!(orbital_verify_bridge_set_route(handle, route.as_ptr()), 1);
            assert_eq!(http(port, "GET", "/snapshot", None).1["route"], "/tasks/42");
            assert_eq!(http(port, "GET", "/route", None).1["route"], "/tasks/42");
            let next = cstr("/");
            orbital_verify_bridge_set_route(handle, next.as_ptr());
            assert_eq!(http(port, "GET", "/route", None).1["route"], "/");
            assert_eq!(orbital_verify_bridge_set_route(handle, std::ptr::null()), 1);
            assert!(http(port, "GET", "/route", None).1["route"].is_null());
            assert_eq!(http(port, "POST", "/route", None).0, 405);
            assert_eq!(
                orbital_verify_bridge_set_route(std::ptr::null_mut(), route.as_ptr()),
                0
            );
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn a_client_only_dispatch_leaves_pending_at_zero() {
        unsafe {
            let (handle, port) = started();
            let wire = dispatch(handle, "TaskCRUD", "CREATE");
            assert!(wire["serverLegs"].as_array().unwrap().is_empty());
            assert_eq!(http(port, "GET", "/idle", None).1["pending"], 0);
            orbital_kernel_destroy(handle);
        }
    }

    fn local_seed_downstream_persist_schema_json() -> String {
        serde_json::json!({
            "name": "App",
            "orbitals": [{
                "name": "Main",
                "entity": { "name": "Message", "fields": [
                    { "name": "id", "type": "string" },
                    { "name": "content", "type": "string" }
                ] },
                "traits": [
                    {
                        "name": "Composer",
                        "linkedEntity": "Message",
                        "local": true,
                        "stateMachine": {
                            "states": [{ "name": "ready", "isInitial": true }],
                            "transitions": [
                                { "from": "ready", "to": "ready", "event": "DRAFT",
                                  "effects": [["set", "@entity.content", "@payload.value"]] },
                                { "from": "ready", "to": "ready", "event": "SEND",
                                  "effects": [["emit", "SAVE", { "content": "@entity.content" }]] }
                            ]
                        }
                    },
                    {
                        "name": "Persistor",
                        "linkedEntity": "Message",
                        "listens": [{ "event": "SAVE", "source": { "kind": "trait", "trait": "Composer" }, "triggers": "DO_CREATE" }],
                        "stateMachine": {
                            "states": [{ "name": "idle", "isInitial": true }],
                            "transitions": [
                                { "from": "idle", "to": "idle", "event": "DO_CREATE",
                                  "effects": [["persist", "create", "Message", { "content": "@payload.content" }]] }
                            ]
                        }
                    }
                ]
            }]
        })
        .to_string()
    }

    unsafe fn started_local_seed() -> (*mut OrbitalKernelHandle, u16) {
        let schema = cstr(&local_seed_downstream_persist_schema_json());
        let client = cstr("verify-test");
        let handle = orbital_kernel_init(schema.as_ptr(), client.as_ptr());
        assert!(!handle.is_null());
        let port = orbital_verify_bridge_start(handle, 0);
        assert!(port > 0, "bridge failed to start");
        assert_eq!(orbital_verify_bridge_set_ready(handle, 1), 1);
        (handle, port as u16)
    }

    // A `local` seed whose cascade reaches a persistor still produces a leg under
    // `hybridClientOnly`; the host must post it (apply) or abandon it, never skip it.
    #[test]
    fn a_local_seed_with_a_downstream_persist_leg_is_pending_until_applied() {
        unsafe {
            let (handle, port) = started_local_seed();
            let wire = dispatch(handle, "Composer", "SEND");
            assert_eq!(wire["mode"], "hybridClientOnly");
            assert!(
                !wire["serverLegs"].as_array().unwrap().is_empty(),
                "cascade persist leg: {wire}"
            );
            assert_eq!(http(port, "GET", "/idle", None).1["pending"], 1);

            let response =
                cstr(r#"{"success":true,"transitioned":false,"states":{},"emittedEvents":[]}"#);
            let already = cstr("[]");
            let out =
                orbital_kernel_apply_server_response(handle, response.as_ptr(), already.as_ptr());
            orbital_kernel_free_string(out);
            assert_eq!(http(port, "GET", "/idle", None).1["pending"], 0);
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn a_local_seed_with_a_downstream_persist_leg_is_pending_until_abandoned() {
        unsafe {
            let (handle, port) = started_local_seed();
            dispatch(handle, "Composer", "SEND");
            assert_eq!(http(port, "GET", "/idle", None).1["pending"], 1);
            abandon(handle, "Composer", "SEND", "server unreachable");
            assert_eq!(http(port, "GET", "/idle", None).1["pending"], 0);
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn a_local_seed_with_no_server_work_never_raises_pending() {
        unsafe {
            let (handle, port) = started_local_seed();
            let wire = dispatch(handle, "Composer", "DRAFT");
            assert_eq!(wire["mode"], "hybridClientOnly");
            assert!(wire["serverLegs"].as_array().unwrap().is_empty());
            assert_eq!(http(port, "GET", "/idle", None).1["pending"], 0);
            orbital_kernel_destroy(handle);
        }
    }

    fn listen_delivery_schema_json(
        listener_effects: serde_json::Value,
        second_persistor: bool,
    ) -> String {
        let mut traits = vec![
            serde_json::json!({
                "name": "Emitter", "linkedEntity": "Note", "local": true,
                "stateMachine": {
                    "states": [{ "name": "ready", "isInitial": true }],
                    "transitions": [{ "from": "ready", "to": "ready", "event": "GO",
                        "effects": [["emit", "PING", { "id": "n1" }]] }]
                }
            }),
            serde_json::json!({
                "name": "Listener", "linkedEntity": "Note",
                "listens": [{ "event": "PING", "triggers": "HEARD", "source": { "kind": "trait", "trait": "Emitter" } }],
                "stateMachine": {
                    "states": [{ "name": "ready", "isInitial": true }],
                    "transitions": [{ "from": "ready", "to": "ready", "event": "HEARD", "effects": listener_effects }]
                }
            }),
        ];
        if second_persistor {
            traits.push(serde_json::json!({
                "name": "Audit", "linkedEntity": "Note",
                "listens": [{ "event": "PING", "triggers": "AUDIT", "source": { "kind": "trait", "trait": "Emitter" } }],
                "stateMachine": {
                    "states": [{ "name": "ready", "isInitial": true }],
                    "transitions": [{ "from": "ready", "to": "ready", "event": "AUDIT",
                        "effects": [["persist", "create", "Note", { "title": "a" }]] }]
                }
            }));
        }
        serde_json::json!({
            "name": "App",
            "orbitals": [{
                "name": "Main",
                "entity": { "name": "Note", "fields": [{ "name": "id", "type": "string" }, { "name": "title", "type": "string" }] },
                "traits": traits
            }]
        })
        .to_string()
    }

    unsafe fn started_with(schema_json: &str) -> (*mut OrbitalKernelHandle, u16) {
        let schema = cstr(schema_json);
        let client = cstr("verify-test");
        let handle = orbital_kernel_init(schema.as_ptr(), client.as_ptr());
        assert!(!handle.is_null());
        let port = orbital_verify_bridge_start(handle, 0);
        assert!(port > 0, "bridge failed to start");
        assert_eq!(orbital_verify_bridge_set_ready(handle, 1), 1);
        (handle, port as u16)
    }

    #[test]
    fn a_listen_delivery_that_leaves_its_trait_in_place_is_journaled_with_event_and_payload() {
        unsafe {
            let (handle, port) =
                started_with(&listen_delivery_schema_json(serde_json::json!([]), false));
            dispatch(handle, "Emitter", "GO");
            let (_, snap) = http(port, "GET", "/snapshot", None);
            let heard = snap["transitions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["traitName"] == "Listener")
                .unwrap_or_else(|| panic!("no Listener trace: {snap}"));
            assert_eq!(
                (
                    heard["from"].as_str(),
                    heard["to"].as_str(),
                    heard["event"].as_str()
                ),
                (Some("ready"), Some("ready"), Some("HEARD"))
            );
            let (_, events) = http(port, "GET", "/events?since=0", None);
            assert!(
                events["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["type"] == "HEARD" && e["source"]["trait"] == "Listener"),
                "{events}"
            );
            orbital_kernel_destroy(handle);
        }
    }

    fn effect_result(kind: &str, action: &str) -> ServerEffectResult {
        serde_json::from_value(
            serde_json::json!({ "effect": kind, "action": action, "success": true }),
        )
        .unwrap()
    }

    #[test]
    fn a_trace_names_an_action_only_for_a_persist_mutation() {
        let set = server_effect_trace(&effect_result("set", "@entity.id"));
        assert_eq!(set.action, None, "a set's target is not a persist mutation");
        let persist = server_effect_trace(&effect_result("persist", "create"));
        assert_eq!(
            persist.action.as_deref(),
            Some("create"),
            "control: persist keeps its mutation"
        );
    }

    #[test]
    fn an_emit_its_own_trait_consumes_is_journaled_once_like_the_web_bus() {
        let schema = serde_json::json!({
            "name": "App",
            "orbitals": [{
                "name": "Main",
                "entity": { "name": "Note", "fields": [{ "name": "id", "type": "string" }] },
                "traits": [{
                    "name": "Board", "linkedEntity": "Note", "local": true,
                    "stateMachine": {
                        "states": [{ "name": "play", "isInitial": true }, { "name": "done" }],
                        "transitions": [
                            { "from": "play", "to": "play", "event": "PUTT", "effects": [["emit", "HOLE_IN"]] },
                            { "from": "play", "to": "done", "event": "HOLE_IN", "effects": [] }
                        ]
                    }
                }]
            }]
        })
        .to_string();
        unsafe {
            let (handle, port) = started_with(&schema);
            dispatch(handle, "Board", "PUTT");
            let (_, events) = http(port, "GET", "/events?since=0", None);
            let holes = events["entries"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["type"] == "HOLE_IN")
                .count();
            assert_eq!(holes, 1, "{events}");
            let (_, snap) = http(port, "GET", "/snapshot", None);
            assert!(
                snap["transitions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["event"] == "HOLE_IN" && t["to"] == "done"),
                "the delivery is still a traced transition: {snap}"
            );
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn control_a_listener_that_never_heard_the_event_is_not_journaled() {
        unsafe {
            let (handle, port) =
                started_with(&listen_delivery_schema_json(serde_json::json!([]), false));
            dispatch(handle, "Listener", "HEARD");
            let (_, snap) = http(port, "GET", "/snapshot", None);
            let transitions = snap["transitions"].as_array().unwrap();
            assert_eq!(transitions.len(), 1, "only the direct dispatch: {snap}");
            assert_eq!(transitions[0]["traitName"], "Listener");
            let (_, events) = http(port, "GET", "/events?since=0", None);
            assert!(!events["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["type"] == "PING"));
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn each_server_bound_listener_leg_raises_pending_once() {
        unsafe {
            let (handle, port) = started_with(&listen_delivery_schema_json(
                serde_json::json!([["persist", "create", "Note", { "title": "l" }]]),
                true,
            ));
            let wire = dispatch(handle, "Emitter", "GO");
            assert_eq!(wire["serverLegs"].as_array().unwrap().len(), 2, "{wire}");
            assert_eq!(http(port, "GET", "/idle", None).1["pending"], 2);
            let response =
                cstr(r#"{"success":true,"transitioned":false,"states":{},"emittedEvents":[]}"#);
            let already = cstr("[]");
            for expected in [1, 0] {
                let out = orbital_kernel_apply_server_response(
                    handle,
                    response.as_ptr(),
                    already.as_ptr(),
                );
                orbital_kernel_free_string(out);
                assert_eq!(http(port, "GET", "/idle", None).1["pending"], expected);
            }
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn restore_lowers_pending() {
        unsafe {
            let (handle, port) = started();
            dispatch(handle, "TaskCRUD", "INIT");
            assert_eq!(http(port, "GET", "/idle", None).1["pending"], 1);
            let t = cstr("TaskCRUD");
            let snapshot = cstr(r#"{"state":"listing","entity":{}}"#);
            let repaint =
                orbital_kernel_restore(handle, t.as_ptr(), std::ptr::null(), snapshot.as_ptr());
            crate::kernel_c_api::orbital_kernel_free_string(repaint);
            assert_eq!(http(port, "GET", "/idle", None).1["pending"], 0);
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn applied_server_transitions_record_a_trace_with_the_server_response() {
        let recorder = Recorder::new(Vec::new());
        recorder.set_enabled(true);
        let result = ProcessEventResult {
            executed: true,
            previous_state: "creating".into(),
            new_state: "listing".into(),
            effects_executed: 0,
            effect_results: Vec::new(),
            entity_data: CoreValue::Null,
            linked_entity: "Task".into(),
            event: Some("SAVE".into()),
            payload: CoreValue::Null,
            entry: None,
            fired_arm: None,
            guards: Vec::new(),
            effect_runs: Vec::new(),
            cascade_dropped: Vec::new(),
        };
        let blocked = ProcessEventResult {
            executed: false,
            ..result.clone()
        };
        recorder.record_applied(
            &|_: &str| "Orb".to_string(),
            &quiet_response(),
            &[
                ("TaskCRUD".to_string(), result),
                ("Other".to_string(), blocked),
            ],
        );
        let (traces, _) = recorder.transitions();
        assert_eq!(traces.len(), 1);
        assert_eq!(
            (
                traces[0].from.as_str(),
                traces[0].to.as_str(),
                traces[0].event.as_str()
            ),
            ("creating", "listing", "SAVE")
        );
        assert_eq!(
            traces[0].server_response.as_ref().unwrap().orbital_name,
            "Orb"
        );
        assert_eq!(recorder.events_since(0).entries[0].event_type, "SAVE");
    }

    #[test]
    fn a_server_denied_persist_is_journaled_on_the_applied_response() {
        let recorder = Recorder::new(Vec::new());
        recorder.set_enabled(true);
        let response = EventResponse {
            success: true,
            effect_results: Some(vec![orbital_core::ServerEffectResult {
                effect: orbital_core::ServerEffectKind::Persist,
                action: Some("create".into()),
                entity_type: Some("Note".into()),
                name: None,
                data: None,
                success: false,
                denied: Some(true),
                error: Some("access denied".into()),
                code: None,
            }]),
            ..Default::default()
        };
        recorder.record_applied(&|_: &str| "Orb".to_string(), &response, &[]);
        let entries = recorder.events_since(0).entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_type, SERVER_EFFECT_EVENT);
        let payload = entries[0].payload.clone().unwrap();
        assert_eq!(payload["outcome"], "denied");
        assert_eq!(payload["error"], "access denied");
    }

    #[test]
    fn control_an_applied_response_with_no_effect_results_journals_none() {
        let recorder = Recorder::new(Vec::new());
        recorder.set_enabled(true);
        recorder.record_applied(&|_: &str| "Orb".to_string(), &quiet_response(), &[]);
        assert!(recorder.events_since(0).entries.is_empty());
    }

    unsafe fn apply_wire(handle: *mut OrbitalKernelHandle, response: &str) -> Value {
        let response = cstr(response);
        let already = cstr("[]");
        let out = orbital_kernel_apply_server_response(handle, response.as_ptr(), already.as_ptr());
        let wire = serde_json::from_str(CStr::from_ptr(out).to_str().unwrap()).unwrap();
        orbital_kernel_free_string(out);
        wire
    }

    #[test]
    fn the_applied_wire_carries_the_servers_denied_persist() {
        unsafe {
            let (handle, port) = started();
            let wire = apply_wire(
                handle,
                r#"{"success":true,"transitioned":false,"states":{},"emittedEvents":[],"effectResults":[{"effect":"persist","action":"create","entityType":"Task","success":false,"denied":true,"error":"access denied"}]}"#,
            );
            let results = wire["response"]["effectResults"]
                .as_array()
                .expect("effectResults");
            assert_eq!(
                (results[0]["denied"].clone(), results[0]["error"].clone()),
                (true.into(), "access denied".into())
            );
            let (_, events) = http(port, "GET", "/events?since=0", None);
            assert!(events["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["type"] == SERVER_EFFECT_EVENT));
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn control_the_applied_wire_of_an_answer_with_no_outcomes_has_no_effect_results() {
        unsafe {
            let (handle, port) = started();
            let wire = apply_wire(
                handle,
                r#"{"success":true,"transitioned":false,"states":{},"emittedEvents":[]}"#,
            );
            assert!(wire["response"].get("effectResults").is_none(), "{wire}");
            let (_, events) = http(port, "GET", "/events?since=0", None);
            assert!(!events["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["type"] == SERVER_EFFECT_EVENT));
            orbital_kernel_destroy(handle);
        }
    }

    fn applied_event_payload(payload: CoreValue) -> Option<serde_json::Value> {
        let recorder = Recorder::new(Vec::new());
        recorder.set_enabled(true);
        let result = ProcessEventResult {
            executed: true,
            previous_state: "a".into(),
            new_state: "b".into(),
            effects_executed: 0,
            effect_results: Vec::new(),
            entity_data: CoreValue::Null,
            linked_entity: "Task".into(),
            event: Some("DataListLoaded".into()),
            payload,
            entry: None,
            fired_arm: None,
            guards: Vec::new(),
            effect_runs: Vec::new(),
            cascade_dropped: Vec::new(),
        };
        recorder.record_applied(
            &|_: &str| "Orb".to_string(),
            &quiet_response(),
            &[("List".to_string(), result)],
        );
        recorder.events_since(0).entries[0].payload.clone()
    }

    #[test]
    fn an_applied_outcome_journals_the_payload_it_was_delivered() {
        let payload: CoreValue =
            serde_json::from_value(serde_json::json!({ "data": [{ "id": "n1" }] })).unwrap();
        assert_eq!(
            applied_event_payload(payload),
            Some(serde_json::json!({ "data": [{ "id": "n1" }] }))
        );
    }

    #[test]
    fn control_an_applied_outcome_without_a_payload_journals_none() {
        assert_eq!(applied_event_payload(CoreValue::Null), None);
    }

    #[test]
    fn entities_returns_seeded_rows_and_an_unknown_name_returns_none() {
        unsafe {
            let (handle, port) = started();
            let entity = cstr("Task");
            let rows = cstr(r#"[{"id":"n1","title":"hello"}]"#);
            orbital_kernel_seed(handle, entity.as_ptr(), rows.as_ptr());
            let (status, body) = http(port, "GET", "/entities?name=Task", None);
            assert_eq!(status, 200);
            assert_eq!(body["rows"][0]["title"], "hello");
            assert_eq!(
                http(port, "GET", "/entities?name=Nope", None).1["rows"],
                serde_json::json!([])
            );
            assert_eq!(http(port, "GET", "/entities", None).0, 400);
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn state_for_an_unknown_trait_is_null() {
        unsafe {
            let (handle, port) = started();
            assert_eq!(
                http(port, "GET", "/state?trait=Nope", None).1["state"],
                Value::Null
            );
            assert_eq!(http(port, "GET", "/state", None).0, 400);
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn unknown_paths_are_404_and_wrong_methods_405() {
        unsafe {
            let (handle, port) = started();
            assert_eq!(http(port, "GET", "/nope", None).0, 404);
            assert_eq!(http(port, "POST", "/health", None).0, 405);
            assert_eq!(http(port, "GET", "/send", None).0, 405);
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn an_oversized_body_is_rejected_with_413() {
        unsafe {
            let (handle, port) = started();
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let head = format!(
                "POST /send HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
                MAX_BODY_BYTES + 1
            );
            stream.write_all(head.as_bytes()).unwrap();
            let mut raw = String::new();
            stream.read_to_string(&mut raw).unwrap();
            assert!(raw.starts_with("HTTP/1.1 413"), "got {raw}");
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn percent_decoding_handles_escapes_and_a_trailing_percent() {
        assert_eq!(percent_decode("a%20b+c"), "a b c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    unsafe fn started_ticking() -> (*mut OrbitalKernelHandle, u16) {
        let schema = cstr(&crate::resolved_fixture("tick-counter"));
        let handle = orbital_kernel_init(schema.as_ptr(), cstr("verify-test").as_ptr());
        assert!(!handle.is_null());
        let port = orbital_verify_bridge_start(handle, 0);
        assert!(port > 0, "bridge failed to start");
        assert_eq!(orbital_verify_bridge_set_ready(handle, 1), 1);
        (handle, port as u16)
    }

    unsafe fn frame(handle: *mut OrbitalKernelHandle, now_ms: u64) {
        let out = orbital_kernel_frame(handle, now_ms);
        if !out.is_null() {
            orbital_kernel_free_string(out);
        }
    }

    #[test]
    fn ticks_are_counted_and_stamped_and_never_hold_the_bridge_busy() {
        unsafe {
            let (handle, port) = started_ticking();
            dispatch(handle, "Ticker", "INIT");
            dispatch(handle, "Listener", "INIT");
            let (_, before) = http(port, "GET", "/snapshot", None);
            assert_eq!(
                before["tickFires"],
                serde_json::json!([]),
                "control: nothing fired yet"
            );

            frame(handle, 0);
            frame(handle, 1000);

            let (_, idle) = http(port, "GET", "/idle", None);
            assert_eq!(
                (idle["idle"].clone(), idle["pending"].clone()),
                (serde_json::json!(true), serde_json::json!(0)),
                "a tick (even one that emitted) is not pending work"
            );
            let (_, snap) = http(port, "GET", "/snapshot", None);
            let fires = |tick: &str| {
                snap["tickFires"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|f| f["tick"] == tick)
                    .map(|f| f["fires"].clone())
            };
            assert_eq!(fires("beat"), Some(serde_json::json!(1)), "{snap}");
            assert_eq!(fires("frameTick"), Some(serde_json::json!(2)), "{snap}");
            let (_, events) = http(port, "GET", "/events", None);
            let beat = events["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["type"] == "BEAT")
                .unwrap_or_else(|| panic!("tick emit not journaled: {events}"));
            assert_eq!(
                beat["source"],
                serde_json::json!({ "trait": "Ticker", "tick": "beat" })
            );
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn a_user_dispatch_event_carries_no_tick_stamp() {
        unsafe {
            let (handle, port) = started_ticking();
            dispatch(handle, "Ticker", "INIT");
            let (_, events) = http(port, "GET", "/events", None);
            let init = events["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["type"] == "INIT")
                .unwrap();
            assert_eq!(init["source"], serde_json::json!({ "trait": "Ticker" }));
            orbital_kernel_destroy(handle);
        }
    }

    #[test]
    fn verify_bridge_symbols_match_declared_fns() {
        let src = include_str!("kernel_c_api.rs");
        for sym in VERIFY_BRIDGE_C_SYMBOLS {
            assert!(src.contains(&format!("fn {sym}(")), "no `fn {sym}(`");
        }
    }
}
