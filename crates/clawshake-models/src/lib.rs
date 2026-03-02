//! Model proxy layer for sharing private LLMs over the clawshake P2P network.
//!
//! This crate provides:
//!
//! - **Backend adapter** — talks to a local model server (Ollama, vLLM, etc.)
//!   via its OpenAI-compatible API and translates responses into
//!   [`StreamFrame`](clawshake_core::stream::StreamFrame)s.
//!
//! - **Local proxy** — an OpenAI-compatible HTTP server that routes
//!   `POST /v1/chat/completions` requests to remote peers over P2P.
//!   Any application that speaks the OpenAI API works unmodified — just
//!   set `OPENAI_BASE_URL=http://127.0.0.1:11435/v1`.
//!
//! The crate is feature-gated in the unified `clawshake` binary so users
//! who don't need model sharing pay zero compile-time or runtime cost.

pub mod backend;
pub mod proxy;
