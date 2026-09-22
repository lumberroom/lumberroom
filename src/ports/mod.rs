//! Ports: what the services need the outside world to provide.
//!
//! Nothing here names SQL, a table, a driver or an engine. A second storage implementation
//! satisfies these same traits and the callers do not change, which makes the adapter test suite
//! the specification any future implementation has to meet.
//!
//! The one genuinely hard case is ranked search. Blending vector distance with lexical rank is
//! expressed differently on every engine, so the port promises ranked results and leaves the
//! ranking to the adapter rather than leaking a query shape upward.
//!
//! Split one file per port so that work on the memory store and work on the authorization server
//! never touch the same file.

pub mod alias;
pub mod cleanup;
pub mod embedder;
pub mod ingest;
pub mod memory;
pub mod oauth;
pub mod registry;
pub mod sealed;
pub mod tool_calls;

pub use alias::{Alias, AliasRepository, NewAlias};
pub use cleanup::CleanupRepository;
pub use embedder::Embedder;
pub use ingest::{
    EmissionHit, EmissionProbe, IngestRepository, NewProposal, NewRun, Proposal, ProposalFilter,
    ProposalSource, ProposalState, ProposalUpsert, RunRecord, RunTotals, Watermark,
    WatermarkAdvance,
};
pub use memory::{
    ChainEdits, ChainLink, ChainNeighbours, ConflictPair, DeleteOutcome, DeletePlan, DigestData,
    DigestQuery, DismissedPair, Emission, MemoryRepository, NamespaceRows, NamespaceSummary,
    NeighbourQuery, NewMemory, RecentQuery, RegistrySummary, RestoreRow, SearchQuery, Staleness,
    Timeline, Weights,
};
pub use oauth::{
    AccessTokenRecord, ClientGrantUpdate, CodeOutcome, NewAccessToken, NewAuthCode, NewOauthClient,
    NewRefreshToken, OauthClientRecord, OauthStore, RefreshOutcome,
};
pub use registry::{
    AliasOrigin, RegistryRepository, RegistryUpsert, RegistryVersion, RegistryWrite,
};
pub use sealed::SealedRepository;
pub use tool_calls::{ClientStats, ToolCallRepository, ToolCallStats};
