# Brain ED roadmap

Brain ED helps agents retain useful project context locally and share selected facts explicitly. This plan starts from the 0.2.0 preview. Items below are proposed work, not shipped guarantees, committed dates or a claim of enterprise readiness. Priorities may change after a pilot. Every milestone needs implementation and observed validation before its status changes.

[Русская версия](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/ROADMAP.ru.md) · [0.2.0 release notes](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/release-notes-0.2.0.md)

## Near term — target 0.3

| Direction | Why it matters to users | Evidence needed |
|---|---|---|
| Controlled company pilot | Check whether assistants can answer useful questions about decisions, blockers and task progress without exposing raw conversations. Start with a few consenting machines and narrow projects. | Real-machine trials of enrollment, manifests, publication, refusal, revocation, stale/offline sources and recovery; record gaps and operator effort. |
| Repeatable GitHub CI and release checks | Make an installable version easier to trust and reproduce operationally. The existing workflow is a starting point, not proof of a successful remote run. | Actual GitHub runs tied to exact commits/tags; build both Linux targets, verify checksums and install/upgrade paths, test quick start in isolation. Publishing remains a separate explicit action. |
| Installation on a managed set of machines | Reduce manual setup for administrators while preserving each machine's data and sharing boundaries. | A documented inventory/enrollment/rotation process, configuration previews, staged rollout, backups and rollback. Measure behavior on a small fleet before claiming broad support. No network-shared SQLite. |
| Better distillation without a subscription | Let people use existing local compute or an API instead of depending on Claude Code/Codex subscriptions. Local/API adapters already exist; the next work is ease of use and measured extraction quality. | Tested recipes for local OpenAI-compatible servers and APIs, context/timeout guidance, visible request costs and rejection reasons, resumable runs and synthetic quality comparisons. No bundled model weights or automatic paid calls. |
| Clearer operational feedback | Help users understand whether an agent is connected, capture is partial, a provider is limited or a peer is unavailable. | Diagnostics and recovery walkthroughs tested against failures; keep configured, observed and unknown states distinct. |

## Medium term — after the pilot

| Direction | Why it matters to users | Work and validation |
|---|---|---|
| macOS first, then Windows | Bring the same memory workflow to more developer machines without weakening local privacy. | Follow the portability analysis: common secure file access, ownership/permissions, locks, restore semantics, hook input and scheduling. Add macOS CI and packaging first; Windows needs explicit ACL/SID handling, MSVC builds, installer and native CI. Unsupported security operations must fail closed. |
| Ed25519 signatures for team slices | Let a receiver verify a slice's originating machine beyond a trusted TLS connection. Current TLS + BLAKE3 integrity is not an author signature. | Canonical signed manifests, machine keys and pinning, rotation/revocation, replay/freshness checks and negative tests. Signatures attest origin/integrity, not the truth of a model's conclusion. |
| Biscuit tokens and constrained delegation | Give administrators narrower delegated read access, with project, field, expiry and further-delegation constraints. | A defined authority model, source-side caveat verification, revocation/recovery policy and refusal tests. Current bearer credentials and grants are not Biscuit. |
| Search on the source machine | Answer questions against locally authorized data with less replication and clearer source-side control. | A new typed read-only protocol operation, authenticated identity, bounded results, policy enforcement and request auditing. Keep raw archive text outside team-facts scope; no fallback to raw data. |
| Manager web/mobile panel | Make policy, freshness, authorized answers and missing sources understandable without CLI use. | A scoped read-only first experience with provenance and access checks; mobile-friendly web access before deciding whether a native app is justified. Keep it distinct from the current local settings UI. |

## Longer term — evidence-led expansion

| Direction | Why it matters to users | Conditions |
|---|---|---|
| Relay to optional P2P and NAT traversal | Reduce dependence on a reachable company relay and simplify connections across real networks. | Evaluate P2P/Iroh and NAT traversal against direct TLS and relay fallback. Test restrictive NATs/firewalls, identity binding, reconnection, resource limits and failure modes. No assumption that hole punching always succeeds. |
| Larger deployments and relay operations | Make wider rollouts maintainable, with predictable load and recovery. | Pilot-derived load targets, credential lifecycle, deployment observability, upgrade/rollback drills and long-running multi-host tests. HA/discovery/automatic provisioning need separate designs and evidence. |
| Broader platform and release assurance | Help users judge supported platforms and exact downloadable artifacts. | Expand native CI as ports mature; assess artifact signing/provenance and independent reproducibility separately from team-slice signatures. Publish only support claims backed by executed checks. |

## Design sources and boundaries

- [Portability analysis](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/portability.md): macOS → Windows, filesystem/security/scheduler and packaging work.
- [ADR 0002](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/adr/0002-team-facts.en.md): phase-two Ed25519/Biscuit and later source-machine search. The ADR contains historical design proposals; current behavior is described in [team facts](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/team-mode-facts.md).
- [Relay](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/relay.md): what exists today and which NAT/P2P/operations checks remain.
- [Distillation](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/distill-guide.md), [provider reference](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/distiller.en.md), [release audit](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/release-audit-0.2.md) and [release builds](https://github.com/vavavadusik-crypto/brain-ed/blob/v0.2.0/docs/build-release.md): current capabilities and evidence limits.

Local-only defaults, explicit recipients/spaces, untrusted archive content, bounded retrieval and provenance remain design constraints throughout. This plan does not introduce screen surveillance, arbitrary-file monitoring or raw archive access for managers.
