# Integration Ledger 2026-03-03

SCOPE_CLAIM worker-classify feat/error-classify crates/agents/src/classify.rs crates/agents/src/runner.rs crates/agents/src/provider_chain.rs
READY worker-classify feat/error-classify 0843572672ac0336b5504192e642c29208a97ac8 moltis-agents tests=pass dependency-note="2.1 and 2.2 can now branch from this"
SCOPE_CLAIM worker-ratelimiter feat/provider-rate-limiter crates/agents/src/rate_limiter.rs crates/agents/src/provider_chain.rs(rate-limit-integration) crates/config/src/schema.rs crates/config/src/validate.rs
READY worker-ratelimiter feat/provider-rate-limiter 1c1ec2d48befe16ab90048c94587fba165b36178 moltis-agents,moltis-config tests=pass dependency-note="branched-from feat/error-classify"
