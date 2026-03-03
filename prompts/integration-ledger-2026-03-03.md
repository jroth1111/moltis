# Integration Ledger 2026-03-03

SCOPE_CLAIM worker-classify feat/error-classify crates/agents/src/classify.rs crates/agents/src/runner.rs crates/agents/src/provider_chain.rs
READY worker-classify feat/error-classify 0843572672ac0336b5504192e642c29208a97ac8 moltis-agents tests=pass dependency-note="2.1 and 2.2 can now branch from this"
SCOPE_CLAIM worker-circuitbreaker feat/circuit-breaker-per-class crates/plugins/src/bundled/circuit_breaker.rs crates/common/src/hooks.rs(AfterLLMCall.error_message) crates/agents/src/classify.rs(Hash) crates/agents/src/runner.rs(error_message) crates/metrics/src/definitions.rs(circuit_breaker)
READY worker-circuitbreaker feat/circuit-breaker-per-class 6fb5e1026f2fe0a2aae88a09f1192e7e6297808e moltis-plugins tests=pass dependency-note="branched-from feat/error-classify"
