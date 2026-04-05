# Agent Collaboration Landscape — Research (Feb 2026)

> Research into multi-agent context sharing, orchestration, and coordination tooling.
> How the ecosystem relates to ATM and where ATM fits.

---

## The Two Layers

The agent collaboration ecosystem splits into two distinct layers:

1. **Shared Memory / Context** — tools that give agents persistent cross-session knowledge
2. **Orchestration / Observability** — tools that manage, monitor, or coordinate agents

These are complementary. You'd use layer 2 to see and coordinate agents, and layer 1 to give them shared knowledge.

---

## Layer 1: Shared Memory / Context Sharing

### Beads
- **Repo:** https://github.com/steveyegge/beads
- **Author:** Steve Yegge
- **Approach:** Git-backed task graph — a machine-readable issue tracker for agents
- **Storage:** SQLite + JSONL in `.beads/`, stored in-repo, versioned with git
- **How agents share:** Read/write a shared dependency DAG of tasks
- **Key features:**
  - `bd ready` surfaces unblocked high-priority tasks
  - Four dependency types: blocks, related, parent-child, replies_to
  - Memory decay summarizes old closed tasks to save context
  - `--json` interface designed for agents as primary users
- **Ecosystem:** beads_viewer (TUI with PageRank, critical path), opencode-beads, Rust port
- **Mental model:** Shared Jira board for agents

### OneContext
- **Repo:** https://github.com/TheAgentContextLab/OneContext
- **Author:** Junde Wu (TheAgentContextLab)
- **Approach:** Persistent context layer — records and replays agent trajectories
- **First release:** Feb 7, 2026 (very new)
- **How it works:**
  1. **Record** — wraps agent, captures execution trajectory
  2. **Share** — generate URL to context, shareable via Slack
  3. **Load** — new agent session loads previous context, picks up where left off
- **Key features:**
  - Works with Claude Code and Codex
  - MCP Server mode
  - Sessions nest within contexts (many sessions per context)
  - Archive/restore workflows
- **Mental model:** Session recording/playback system

### Cipher
- **Repo:** https://github.com/campfirein/cipher
- **Author:** ByteRover (campfirein)
- **Stars:** 3.5k (most mature by commit count — 791 commits)
- **Approach:** Three-tier memory layer with knowledge graph
- **Architecture:**
  - **System 1 Memory (Knowledge)** — vector embeddings for semantic search over codebase knowledge, business logic, past interactions
  - **System 2 Memory (Reasoning)** — captures AI reasoning traces (how decisions were made, not just what)
  - **Workspace Memory (Team)** — shared across team members with scoped access
- **Storage:** Vector DB (Qdrant/Milvus/in-memory) + Knowledge Graph + PostgreSQL/SQLite
- **MCP tools:** 20+ tools including memory CRUD, reasoning pattern search, knowledge graph operations, workspace queries
- **Server modes:** Default (conversational only) and Aggregator (full tool access, can proxy other MCP servers)
- **Transport:** STDIO, SSE, Streamable HTTP
- **IDE support:** 12+ (Cursor, Claude Code, Gemini CLI, VS Code, Kiro, Roo Code, etc.)
- **Mental model:** Kahneman's Thinking Fast and Slow — System 1 (pattern matching) + System 2 (deliberate reasoning)

### Letta Code
- **Repo:** https://github.com/letta-ai/letta-code
- **Approach:** Memory-first agent with git-backed context repositories
- **Key features:**
  - Context repositories with git versioning (every memory change committed)
  - Skill learning — agent learns from coaching, shares skills with others
  - Concurrent subagent collaboration via git-backed memory
- **Mental model:** Agent that accumulates expertise over time

### MCP Agent Mail
- **Repo:** https://github.com/Dicklesworthstone/mcp_agent_mail
- **Approach:** Async messaging layer for agents
- **Key features:** Identities, inboxes, searchable threads, advisory file leases
- **Complement to Beads:** Beads = shared memory, Agent Mail = direct messaging

### Comparison

| Dimension | Beads | OneContext | Cipher |
|-----------|-------|-----------|--------|
| **What's stored** | Tasks + dependencies | Agent trajectories | Knowledge + reasoning + graphs |
| **Structure** | High (dependency DAG) | Low (full context) | Medium (semantic + graph) |
| **Context cost** | Cheap (query specific tasks) | Expensive (full load) | Medium (relevant memories) |
| **Learning** | No | No (replay only) | Yes (reasoning patterns) |
| **Auditability** | Strong | Weak | Medium |
| **Scalability** | Good (memory decay) | Unclear (trajectories grow) | Good (vector similarity) |

---

## Layer 2: Orchestration / Observability

### ATM (this project)
- **Approach:** Passive TUI monitor for tmux-based agent sessions
- **Key:** Non-intrusive, PID-based tracking, framework-agnostic
- **Analogy:** `htop` for agents

### Overcode
- **Repo:** https://github.com/mkb23/overcode
- **Approach:** TUI supervisor — launches, manages, and monitors agents
- **Key features:**
  - Standing orders per agent
  - Auto-approve daemon for routine prompts
  - Web dashboard + analytics + Parquet export
  - Cost/token tracking, git info
- **Analogy:** `systemd` for agents

### Gas Town
- **Repo:** https://github.com/steveyegge/gastown
- **Author:** Steve Yegge
- **Approach:** Multi-agent workspace manager for Claude Code (20-30+ agents)
- **Architecture:** Hierarchical roles — Mayor, Polecats (workers), Refinery (merges), Witness, Deacon (health), Dogs, Crew
- **File coordination:** Git worktree isolation (each agent gets own worktree)
- **Task tracking:** Beads integration
- **Merge strategy:** Refinery agent handles merge queue with rebase, CI-gated
- **Philosophy:** GUPP (Git Up Pull Push) — deterministic handoffs through git

### Swarm Tools
- **Repo:** https://github.com/joelhooks/swarm-tools
- **Author:** Joel Hooks
- **Stack:** TypeScript/Bun, libSQL, Turbo monorepo
- **Architecture:** Three-tier (Primitives → Coordination → Orchestration)
- **Components:**
  - **Hive** — git-backed task tracker in `.hive/` (8 MCP tools)
  - **Swarm Mail** — actor-model messaging with DurableMailbox, DurableLock, ask() RPC (6 tools)
  - **Hivemind** — semantic learning with Ollama embeddings (pattern maturity: candidate → established → proven)
  - **File Reservations** — glob-pattern locking for concurrent edit prevention
  - **Checkpoints** — automatic at 25/50/75%, stored in libSQL
- **Learning:** Anti-patterns auto-generate at >60% failure rate, 90-day confidence decay
- **40+ MCP tools** total

### gwq
- **Repo:** https://github.com/d-kuro/gwq
- **Author:** d-kuro
- **Stack:** Go, requires Git 2.5+
- **Approach:** Standalone git worktree manager — decouples worktree isolation from orchestration
- **Key features:**
  - Hierarchical worktree namespace: `~/worktrees/host/owner/repo/branch`
  - Fuzzy finder interface for branch selection and worktree navigation
  - Status dashboard across all worktrees and repositories
  - Tmux integration for persistent per-worktree sessions
  - Per-repository configuration (auto-copy files, setup commands)
  - Filesystem scanning (no registry to maintain)
- **Relevance:** Extracts the worktree isolation primitive that Gas Town bundles internally. Any orchestrator (or no orchestrator) can use gwq for workspace isolation. Predictable path structure (`host/owner/repo/branch`) makes it easy for monitoring tools to detect "same project, different worktrees."
- **Analogy:** `ghq` for worktrees — like how `ghq` manages repo clones, gwq manages worktrees
- **Mental model:** Infrastructure layer that sits between git and orchestration

### Claude Flow (Ruflo v3)
- **Repo:** https://github.com/ruvnet/claude-flow
- **Author:** ruvnet
- **Stack:** TypeScript + WASM, libSQL, MCP
- **Scale:** 60+ specialized agents, 250k+ LoC
- **Architecture:**
  - Q-Learning Router + Mixture of Experts (8 specialized routers)
  - Swarm topologies: hierarchical, mesh, ring, star
  - Consensus: Raft, Byzantine, gossip, weighted voting
  - RuVector: HNSW vector search, PostgreSQL backend
  - Self-learning: SONA, EWC++ (catastrophic forgetting prevention)
  - Agent Booster: WASM for simple transforms (<1ms, no LLM call)
- **File coordination:** Claims system + intelligent routing + Coherence Engine (spectral analysis for contradictory changes)
- **Note:** File coordination mechanics underdocumented despite ambitious claims

---

## Merge Conflict Strategies — The Core Architectural Split

The most important design decision in multi-agent coding is how to handle concurrent file edits. Three approaches exist:

### 1. Optimistic: Worktree Isolation (Gas Town, gwq)
```
Each agent → own git worktree (gwq manages) → edits freely → Refinery merges → CI gates
```
- **Analogy:** MVCC in databases (PostgreSQL, InnoDB)
- **Pros:** Maximum parallelism, no lock contention, git merge is well-understood
- **Cons:** Disk usage (N worktrees), merge queue complexity
- **Failure mode:** Merge conflicts at integration time (routed back to author)
- **Scales to:** 20-30+ agents

### 2. Pessimistic: File Reservations (Swarm Tools)
```
Agent requests lock → acquires exclusive access → works → releases lock
```
- **Analogy:** Two-Phase Locking (2PL) in databases
- **Pros:** Predictable, immediate feedback on conflicts
- **Cons:** Parallelism limited by lock contention, hot path bottlenecks, deadlock risk
- **Failure mode:** Stale locks from crashed agents (handled via TTL)
- **Scales to:** Small swarms (degrades with agent count)

### 3. Preventive: Intelligent Routing (Claude Flow)
```
Router assigns non-overlapping work → Claims system tracks ownership → Coherence Engine catches drift
```
- **Analogy:** Perfect sharding (each agent owns a partition)
- **Pros:** If routing is perfect, no conflicts ever; no disk overhead
- **Cons:** Requires perfect knowledge of file dependencies upfront; agents discover needs during work
- **Failure mode:** Routing misses cross-cutting concerns
- **Scales to:** 60+ agents (claimed, unproven)

### Verdict

Gas Town's worktree isolation is **structurally the most sound**. It mirrors how databases solved the concurrency problem — optimistic concurrency (MVCC) outperforms pessimistic locking (2PL) at scale. Swarm Tools' locking works for small swarms. Claude Flow's prevention is the most ambitious but least proven.

**Ideal hybrid system:**
1. Worktree isolation for the work (Gas Town)
2. Advisory reservations as routing hints (not hard locks)
3. Semantic conflict detection pre-merge (Claude Flow's Coherence Engine)
4. Learning system with confidence decay (Swarm Tools' Hivemind)
5. CI-gated merge queue (Gas Town's Refinery)

---

## Where ATM Fits

```
Layer             Tool               Question it answers
─────────────────────────────────────────────────────────
Observability     ATM                "What are agents doing right now?"
Shared Memory     Beads/Cipher       "What have agents decided/learned?"
Messaging         Agent Mail         "Agents talking to each other"
Coordination      Gas Town/Swarm     "Who does what, when?"
Worktree Mgmt     gwq                "Where does each agent work?"
```

ATM's value proposition: **framework-agnostic observability**. Gas Town, Swarm Tools, and Claude Flow all have monitoring built in, but they only see *their own* agents. ATM watches agents regardless of orchestrator.

### Possible Evolution Path

1. **Current:** Passive monitoring (session list, status, cost)
2. **Projects/Teams:** Group by CWD, show parent-child relationships
3. **Integration:** Read `.beads/` or `.hive/` for task-aware monitoring
4. **Advisory:** Detect potential file conflicts across sessions in real-time
5. **Coordination:** Active warnings, routing suggestions (furthest out)

### Open Questions
- Could ATM detect file conflicts by watching agent activity across sessions?
- Integration with Beads `.beads/` or Hive `.hive/` for task-aware monitoring?
- Should ATM remain purely passive or offer advisory coordination?
- Is there a market for a "universal agent monitor" that works across all orchestrators?
- gwq's predictable `~/worktrees/host/owner/repo/branch` paths make project grouping trivial — ATM could detect "same project, different worktrees" and show per-agent branch status without any configuration
