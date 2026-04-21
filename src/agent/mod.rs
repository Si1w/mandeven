//! Agent module — pending implementation.
//!
//! TODO: streaming tool-call state machine. When tool use lands here,
//! the agent loop will receive interleaved content and tool-call deltas
//! in the same `llm::ResponseStream`. Fragments must be assembled by their
//! stable `index` into complete `llm::ToolCall`s before dispatching to
//! skills.
