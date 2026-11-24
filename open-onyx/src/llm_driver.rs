//! Driver LLM dinámico (`SDD-03` Fase 2): hot-swap de perfil/proveedor/modelo
//! en caliente para `chat`/`serve`.
//!
//! A diferencia de `AsyncGeminiDriver`/`AsyncOpenAiDriver` (fijos al modelo de
//! arranque y solo para tests), `DynamicLlmDriver` congela el **objetivo activo**
//! de `LlmProfileHandle` (provider/modelo/clave/base/esfuerzo) **por petición
//! pendiente, en el hilo principal**, antes de lanzar el hilo HTTP. Un cambio en
//! caliente (`/profile`/`/provider`/`/model`/`/effort` en frontera de tick) solo
//! aplica a la siguiente llamada: ninguna petición HTTP en vuelo se muta.
//!
//! La cola secuencial de tool calls es **compartida** entre los estados Gemini y
//! OpenAI (un solo `PendingCallQueue` Arc), de modo que una continuación de tool
//! call se entregue por el mismo provider aunque el objetivo cambie de perfil.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use onyx_soul_agents::{
    EventBus, GraphView, PlatformDriver, Reply, SurfaceBinding, Vocab, pending_requests,
};

use crate::anthropic::{
    AnthropicSharedState, AnthropicStreamTarget, DEFAULT_ANTHROPIC_API_BASE,
    anthropic_messages_url, build_anthropic_request_body, effective_thinking_budget,
    execute_anthropic_stream_http,
};
use crate::context::{SessionTelemetryHandle, TieredContextConfig};
use crate::gemini::{
    DEFAULT_GEMINI_API_BASE, GeminiSharedState, GeminiStreamTarget, InFlightGuard,
    PendingCallQueue, StreamPreviewHandle, build_request_body_with_context,
    execute_gemini_stream_http, next_queued_call,
};
use crate::llm_profile::{LlmProfileHandle, LlmProvider};
use crate::openai::{
    DEFAULT_OPENAI_API_BASE, OpenAiSharedState, OpenAiStreamTarget, build_openai_request_body,
    execute_openai_stream_http,
};

/// Driver asíncrono con streaming SSE y **objetivo LLM dinámico** (hot-swap en
/// caliente, `SDD-03` Fase 2). Envuelve al driver de subagentes (`wrapping`),
/// igual que hacían `AsyncGeminiDriver`/`AsyncOpenAiDriver` en `chat`/`serve`.
pub struct DynamicLlmDriver {
    vocab: Vocab,
    profile_handle: LlmProfileHandle,
    gemini_shared: GeminiSharedState,
    openai_shared: OpenAiSharedState,
    anthropic_shared: AnthropicSharedState,
    dispatched: HashSet<(u64, u64)>,
    in_flight_count: Arc<AtomicUsize>,
    has_ready: Arc<AtomicBool>,
    stream_preview: Option<StreamPreviewHandle>,
    rx: mpsc::Receiver<Reply>,
    tx: mpsc::Sender<Reply>,
    inner: Option<Box<dyn PlatformDriver>>,
}

impl DynamicLlmDriver {
    pub fn new(
        vocab: Vocab,
        profile_handle: LlmProfileHandle,
        in_flight_count: Arc<AtomicUsize>,
        has_ready: Arc<AtomicBool>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        // Cola de tool calls compartida entre los tres estados de provider.
        let pending_calls = PendingCallQueue::default();
        let gemini_shared = GeminiSharedState {
            pending_calls: Arc::clone(&pending_calls),
            ..GeminiSharedState::default()
        };
        let openai_shared = OpenAiSharedState {
            pending_calls: Arc::clone(&pending_calls),
            ..OpenAiSharedState::default()
        };
        let anthropic_shared = AnthropicSharedState {
            pending_calls,
            ..AnthropicSharedState::default()
        };
        Self {
            vocab,
            profile_handle,
            gemini_shared,
            openai_shared,
            anthropic_shared,
            dispatched: HashSet::new(),
            in_flight_count,
            has_ready,
            stream_preview: None,
            rx,
            tx,
            inner: None,
        }
    }

    pub fn wrapping(mut self, inner: impl PlatformDriver + 'static) -> Self {
        self.inner = Some(Box::new(inner));
        self
    }

    pub fn with_stream_preview(mut self, preview: StreamPreviewHandle) -> Self {
        self.stream_preview = Some(preview);
        self
    }

    pub fn with_telemetry(mut self, telemetry: SessionTelemetryHandle) -> Self {
        self.gemini_shared.telemetry = Some(telemetry.clone());
        self.openai_shared.telemetry = Some(telemetry.clone());
        self.anthropic_shared.telemetry = Some(telemetry);
        self
    }

    pub fn with_external_driver_ids(mut self, driver_ids: Vec<String>) -> Self {
        self.gemini_shared.driver_ids = driver_ids.clone();
        self.openai_shared.driver_ids = driver_ids.clone();
        self.anthropic_shared.driver_ids = driver_ids;
        self
    }
}

impl PlatformDriver for DynamicLlmDriver {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        if let Some(inner) = self.inner.as_mut() {
            inner.poll(bus);
        }
        self.has_ready.store(false, Ordering::Release);
        while let Ok(reply) = self.rx.try_recv() {
            if let Some(prev) = &self.stream_preview {
                prev.clear();
            }
            bus.push(reply.to_event());
        }
    }

    fn present(&mut self, graph: &dyn GraphView, surfaces: &[SurfaceBinding]) {
        if let Some(inner) = self.inner.as_mut() {
            inner.present(graph, surfaces);
        }
        let preview = self.stream_preview.clone();
        for req in pending_requests(graph, &self.vocab) {
            let key = (req.agent.0, req.request);
            if !self.dispatched.insert(key) {
                continue;
            }
            // Continuation de tool call: se entrega sin HTTP (cola compartida).
            if let Some(queued_reply) = next_queued_call(&self.gemini_shared.pending_calls, &req) {
                if self.tx.send(queued_reply).is_ok() {
                    self.has_ready.store(true, Ordering::Release);
                }
                continue;
            }

            // Congela el objetivo activo: un cambio en caliente no muta esta
            // petición (body y URL se construyen síncrono, antes de `thread::spawn`).
            let target = self.profile_handle.active_target();
            let ctx_cfg = TieredContextConfig::for_model(&target.model);
            let is_local = target.is_local_endpoint;
            let model = target.model.clone();
            let api_key = target.api_key.clone();
            let project_instructions = target.project_instructions.clone();
            let effort = target.effort;

            self.in_flight_count.fetch_add(1, Ordering::Release);
            let guard = InFlightGuard(Arc::clone(&self.in_flight_count));
            let thread_preview = preview.clone();
            let agent = req.agent;
            let request = req.request;
            let call_index = req.memory.iter().filter(|(k, _)| k == "call").count();

            match target.provider {
                LlmProvider::Gemini => {
                    let base = target
                        .api_base
                        .clone()
                        .unwrap_or_else(|| DEFAULT_GEMINI_API_BASE.to_string());
                    let url = format!("{base}/models/{model}:streamGenerateContent?alt=sse");
                    let mut shared = self.gemini_shared.clone();
                    shared.context_config = ctx_cfg;
                    shared.effort = effort;
                    shared.project_instructions = project_instructions.clone();
                    let body = {
                        let sigs = shared
                            .thought_signatures
                            .lock()
                            .map(|g| g.clone())
                            .unwrap_or_default();
                        build_request_body_with_context(
                            &req,
                            &sigs,
                            ctx_cfg,
                            &shared.project_instructions,
                            shared.effort,
                            &shared.driver_ids,
                        )
                    };
                    let ready_flag = Arc::clone(&self.has_ready);
                    let tx = self.tx.clone();
                    thread::spawn(move || {
                        let _guard = guard;
                        let reply = execute_gemini_stream_http(
                            GeminiStreamTarget {
                                url: &url,
                                api_key: &api_key,
                                agent,
                                request,
                                call_index,
                                shared: &shared,
                                preview: thread_preview.as_ref(),
                                model: &model,
                                is_local_endpoint: is_local,
                            },
                            body,
                        );
                        if tx.send(reply).is_ok() {
                            ready_flag.store(true, Ordering::Release);
                        }
                    });
                }
                LlmProvider::OpenAi => {
                    let base = target
                        .api_base
                        .clone()
                        .unwrap_or_else(|| DEFAULT_OPENAI_API_BASE.to_string());
                    let url = format!("{base}/chat/completions");
                    let mut shared = self.openai_shared.clone();
                    shared.context_config = ctx_cfg;
                    shared.effort = effort;
                    shared.project_instructions = project_instructions.clone();
                    let body = build_openai_request_body(
                        &req,
                        &model,
                        ctx_cfg,
                        &shared.project_instructions,
                        shared.effort,
                        true,
                        &shared.driver_ids,
                    );
                    let ready_flag = Arc::clone(&self.has_ready);
                    let tx = self.tx.clone();
                    thread::spawn(move || {
                        let _guard = guard;
                        let reply = execute_openai_stream_http(
                            OpenAiStreamTarget {
                                url: &url,
                                api_key: &api_key,
                                agent,
                                request,
                                shared: &shared,
                                preview: thread_preview.as_ref(),
                                model: &model,
                                is_local_endpoint: is_local,
                            },
                            body,
                        );
                        if tx.send(reply).is_ok() {
                            ready_flag.store(true, Ordering::Release);
                        }
                    });
                }
                LlmProvider::Anthropic => {
                    let base = target
                        .api_base
                        .clone()
                        .unwrap_or_else(|| DEFAULT_ANTHROPIC_API_BASE.to_string());
                    let url = anthropic_messages_url(&base);
                    let mut shared = self.anthropic_shared.clone();
                    shared.context_config = ctx_cfg;
                    shared.project_instructions = project_instructions.clone();
                    shared.thinking_budget = target.thinking_budget;
                    let body = {
                        let thinking_blocks = shared
                            .thinking_blocks
                            .lock()
                            .map(|g| g.clone())
                            .unwrap_or_default();
                        let thinking_budget =
                            effective_thinking_budget(shared.thinking_budget, effort);
                        build_anthropic_request_body(
                            &req,
                            &model,
                            ctx_cfg,
                            &shared.project_instructions,
                            true,
                            &shared.driver_ids,
                            &thinking_blocks,
                            thinking_budget,
                        )
                    };
                    let ready_flag = Arc::clone(&self.has_ready);
                    let tx = self.tx.clone();
                    thread::spawn(move || {
                        let _guard = guard;
                        let reply = execute_anthropic_stream_http(
                            AnthropicStreamTarget {
                                url: &url,
                                api_key: &api_key,
                                agent,
                                request,
                                call_index,
                                shared: &shared,
                                preview: thread_preview.as_ref(),
                                model: &model,
                                is_local_endpoint: is_local,
                            },
                            body,
                        );
                        if tx.send(reply).is_ok() {
                            ready_flag.store(true, Ordering::Release);
                        }
                    });
                }
            }
        }
    }
}
