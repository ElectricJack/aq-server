# 0007: The swarm (always-on workers, cloud VMs and swarm controls)

**Status:** Approved by the owner on 2026-11-07 (brainstorming session; builder-check answers folded in). Cloudflare §7.5 follows the recommendation (manual tokens).
**Controls correction, 2026-11-11 (owner):** provider concurrency allocation sliders and burn estimates belong in the collapsible Swarm header; the existing Intelligence settings row gains one Max agents column for the selected provider. This supersedes the earlier Settings › Swarm and Normal/Reduced controls.
**Repositories:** aq-server, aq-worker, aq-dashboard.
**Mockup:** `aq-dashboard/docs/design/swarm-tree-v4.html` (revised controls prototype, labelled sample data). `swarm-tree-v3.html` and `14-swarm-tree-v3.png` retain the earlier tree reference. React Flow plus dagre remain the tree libraries.

Decisions are marked **(owner)** when the owner made them and **(my call)** when the supervisor filled a gap; the owner should check the second kind while reading.

## 1. Intent

AQ should run work on every machine the owner has, plus cloud VMs when those aren't enough, and the owner should be able to see and steer all of it from one page.

Today v2 can run an agent only when a script starts a worker. v1 scales per-project pools on one host. The owner wants:

- a **swarm** made of local machines (desktop, laptop) and cloud VMs (boat), all working the same queue;
- workers that behave like **services**: always on, back after a reboot, and controllable from the dashboard;
- a **swarm page** that shows the whole swarm and has a few intuitive controls for how big it may grow and how much it may burn.

The motivating moment: the owner wants to play games and stops the desktop's worker from the dashboard. Each running agent's work is saved, and a fresh agent picks it up in a cloud VM, with nothing else for the owner to do.

## 2. Goals

1. One page, Agents › Swarm, that shows the swarm as a tree and controls its size.
2. Workers that run as an OS service on Linux and macOS, restart by themselves, and resume or retry interrupted agents.
3. Starting and stopping any worker from the dashboard, with its work moving elsewhere.
4. Adding a machine by running one install script that enrolls it.
5. Boat VMs started and stopped by AQ automatically, within a maximum set in Settings.
6. Per-machine limits (agent count, CPU, memory) so a machine never runs more than it can carry, and the page saying so when it holds work back.

## 3. Non-goals

- **Per-project pool sizes.** They are replaced by the swarm controls **(owner)**. Projects share one swarm.
- **Automatic throttling near provider plan limits.** Manual concurrency limits are in scope (§5.3); automatic adjustment is deferred **(owner, clarified 2026-11-11)**.
- **Windows-native workers.** Only Linux (including WSL2) and macOS are supported **(owner)**.
- **Manual VM management.** The owner does not start or stop VMs by hand.

## 4. Anti-goals

These would count as failure even if everything technically works:

- **Silent holding back.** Work not starting without the page saying why. Every "not now" has a visible reason **(owner)**.
- **A crawling machine.** Overloading a machine until builds crawl, because AQ ignored its limits.
- **Leaking the credential.** An agent credential reaching an agent's sandbox, on any machine.

## 5. The swarm page

### 5.1 Layout

Agents › Swarm becomes the Agents landing page **(owner)**:

1. **Compact, collapsible status header** at the top (§5.3). Provider allocation controls live in the expanded header.
2. **Swarm tree** below it, with a **properties pane** on the right (§5.4). It opens with the whole tree fitted on screen (zoom between 0.5 and 1); a `?select=` link centres on that object **(owner, 2026-11-07)**.

### 5.2 The tree

It is drawn with `@xyflow/react` 12 and laid out by `@dagrejs/dagre`, as in v1 **(owner)**. It is top-down, with the AQ server at the root **(owner)**:

- **AQ server**, with machines below it.
  - **Machine** (local or cloud, visibly distinct). A cloud VM appears as a short-lived machine with exactly one agent, because each cloud VM runs one agent attempt (§7.5) **(owner)**. Each machine card shows:
    - its name and kind;
    - live **CPU and RAM bars** **(owner)**;
    - its agent maximum.
    - **Worker node.** Shows the backend (docker-local, boat) and slots used/total.
      - **Agent.** A pill coloured by intelligence class and ringed by provider.

**States are always visible, never hidden:**
- **Free slot:** a dashed pill **(my call)**.
- **Offline or stopped machine:** greyed and dashed, with a Start button in its pane.
- **Booting VM:** dashed amber.
- **Holding back** (compute-bound or at a limit): amber, with the reason on the card, e.g. "holding: CPU 92% > 85%" **(owner: communicate it)**.
- **Agents waiting for a slot:** a "waiting" branch under the server, each with its reason, e.g. "desktop compute-bound; VM maximum reached".

Pan and zoom are supported. Left-to-right layout is optional **(my call)**.

### 5.3 Status header and quick control

The collapsible Swarm header contains provider concurrency allocation sliders and burn estimates. The global top bar already shows usage; do not repeat usage-window displays in the Swarm header. Show running and waiting context and the saved provider cap, with pending and failed writes visible. The collapsed header retains its compact summary and accessible disclosure. On phones controls may wrap without horizontal page scrolling.

Each provider cap is independent of optional profile/provider overrides. An unset limit (`null`) is unlimited; zero pauses new starts. Lowering a cap does not stop existing agents. Routing remains unchanged and work waits when its required capacity is unavailable.

Estimate account quota points per hour from fresh browser sample history. Project burn at the provider cap only when active-agent calibration supports it; otherwise show gathering or unavailable. These estimates are not monetary cost and account usage outside AQ can affect them.

Start collapsed, remember the viewer's preference on that device, and keep the header bounded to its own content. Expanding or collapsing does not reset tree pan, zoom or selection. Use a keyboard-operable button with `aria-expanded` and `aria-controls`; hidden details leave the tab order.

### 5.4 Properties pane

Clicking any tree object shows its details **(owner)**:

**Server:**
- health and version;
- ready and waiting tasks;
- running agents;
- usage-window levels;
- projects.

**Machine:**
- online, offline or stopped;
- live load and 15-minute average;
- cores, RAM and free disk;
- its limits, with a link to edit them in Settings;
- for a VM: cost per hour, uptime and its idle-stop rule.
- **Actions:** start or stop worker, and drain.

**Worker node:**
- backend, slots and heartbeat;
- worker version and sandbox image;
- whether the agent channel is ready.
- **Actions:** drain, restart, logs.

**Agent:**
- task, attempt, model and tokens;
- the **live terminal, connected with control already taken**. There is no read-only mode **(owner)**.
  - If another viewer holds the control lease, opening the pane takes it over and they see a notice **(my call)**.
  - Viewers without the terminal-control capability get a watch-only view.
- **Actions:** cancel attempt, open task, pop out the terminal.

**Free slot or waiting agent:** why it is free or waiting, and what would change that.

## 6. Settings

Add one **Max agents** column to the existing Intelligence settings row for the selected provider. Each optional profile/provider override limits concurrent agents for that pair across projects and machines. A blank/unset override is unlimited; zero pauses new starts. Show running and waiting context, including when running exceeds a newly lowered limit. Unsupported pairs remain unavailable. There is no separate Swarm settings tab or Normal/Reduced mode UI.

Other settings remain:

- **Cloud:**
  - the maximum number of boat VMs, which is also the maximum number of cloud agents running at once;
  - the VM size.
- **Per machine:**
  - maximum agents **(owner)**;
  - CPU threshold and memory threshold, above which no new agents start there **(owner)**. On the WSL2 desktop these measure load inside the WSL VM, where agents and builds run. Windows host load is not measured: when the owner wants the machine, they stop the worker from the dashboard **(owner, 2026-11-07; replaces the earlier Windows-probe call)**;
  - a display name.
- **Add machine:** shows the one-line install command with a single-use enrollment code **(my call)**.

## 7. Behaviour

### 7.1 Placement

The router selects a task's intelligence class from its requirements and independently determines eligible providers. It never changes the required class to meet a target mix **(owner, 2026-11-11)**. Placement then applies the independent provider cap and any configured provider/class override:

1. **A matching intelligence slot must be available.** Reserve capacity against the provider/class ceiling before admitting work, across concurrent placement decisions. Cloud capacity cannot bypass this check. Eligible-provider fallback remains governed by routing policy, not by changing the intelligence class or borrowing another class's slots.
2. **Which machines are eligible.** An online machine whose worker is running, that is under its agent maximum, and under its CPU and memory thresholds.
3. **Local machines first,** then running VMs.
4. **Overflow to the cloud.** If an intelligence slot is available but no machine is eligible, and running VMs are under the VM maximum, AQ starts a new VM **(owner: automatic scaling)**.
5. **Otherwise the task waits,** retaining its required class. Show the specific reason (§5.2), including `Claude deep concurrency limit reached`, zero-limit pauses, provider usage exhaustion, and machine/VM constraints. Distinguish queued tasks from already running agents.

Limit changes affect new admissions only. Running agents finish; lowering a limit never cancels or reclassifies them. Release reservations when launches fail or attempts finish so a failed start cannot permanently consume a slot. Verify that queued work starts when matching capacity becomes available and that simultaneous admissions cannot exceed the ceiling.

### 7.2 Workers as services

- `aq-worker` installs as a user service **(owner)**:
  - **Linux:** systemd. On WSL2 this needs systemd enabled in WSL, and the installer checks for it.
  - **macOS:** launchd, on both Intel and Apple silicon, with Docker Desktop as the container runtime **(owner)**. This means:
    - multi-arch (amd64 and arm64) sandbox templates and a static `aqv2` for both architectures, so every pinned template digest becomes a per-architecture pair;
    - **risk:** spec 0006 bind-mounts the agent channel's Unix socket into the container, and that is not reliable across Docker Desktop's VM boundary on macOS. The builder must prove it works or design the macOS channel transport before macOS is promised.
- It starts at boot or login and restarts on crash.
- **After a restart** (reboot, sleep, crash), interrupted agents get a **fresh attempt from their last saved work** **(owner)**, through the existing recovery and replacement machinery. "Resume" means continuing from the saved work, not from the conversation.
- **Start and stop from the dashboard** **(owner)**:
  - **Stop:** the worker hands off its running agents **(owner)**:
    1. Each running attempt is stopped.
    2. Its work is saved as a commit, together with a short handoff note.
    3. A **fresh attempt starts elsewhere from that commit**, which can mean starting a VM. The new agent does not inherit the old agent's conversation. This is the existing replacement-attempt machinery (0002 §7).
    4. The worker then stops taking work.
  - **(my call)** "Stop" saves and hands off immediately, because waiting for agents to finish would defeat the gaming case. A separate "Drain" lets running agents finish first.
  - **Start:** the worker takes work again.
  - The service process stays installed and connected while stopped, so the dashboard can start it again.

### 7.3 Adding a machine

The owner runs an install script on the new machine, and it enrolls with the server **(owner)**. The script:
1. installs `aq-worker` and its sandbox runtime;
2. enrolls with the single-use code from Settings;
3. installs and starts the service;
4. measures the machine.

The machine then appears in the tree. Boat VMs run the same enrollment automatically when AQ starts them.

### 7.4 Provider usage limits (interim)

Until the deferred design (§3):
- The header shows each provider window's level where AQ has a source; projected burn is separate from measured usage and appears only with a supported estimate.
  - **Codex:** the existing quota collector.
  - **Claude:** "unknown" until a source exists. Today nothing reads Claude usage.
- When a window is exhausted, that provider's agents **wait for the reset** instead of failing, and the page says so **(my call)**. For Claude, "exhausted" is detected from rate-limit errors reported by the harness.

### 7.5 Boat VMs

- **One VM per agent attempt** **(owner)**, as in spec 0005: AQ starts a VM for an attempt and destroys it when the attempt ends. There is no idle VM pool, so the idle-stop delay in §6 is dropped.
- **The server starts VMs** **(my call)** and holds the Boat API credential, so stopping or switching off any of the owner's machines never stops cloud scaling.
- **Credential custody** follows spec 0006 §5.3. The runner process in the VM holds the agent credential (P7 option A, a channel proxy in the VM reusing the phase 3 code), and the agent's sandbox never sees it **(owner)**.
- **Cloudflare Access** (recommended; owner to confirm):
  - AQ never holds a Cloudflare API credential.
  - The owner creates two Access service tokens by hand:
    - a **node token**, used by the install script on the owner's machines;
    - a **cloud token**, used by VMs, whose Access policy allows only the worker and agent routes.
  - Per-machine identity and revocation come from AQ's own enrollment credentials.
  - The owner rotates both tokens by hand.
- **Cost:** the page shows each VM's cost per hour and uptime.
- **What has to be built** (from aq-worker `docs/implementation/0006-P7.md`, on branch `chan/p7-worker`): today boat has no agent launch path (`UnavailableProvider`), and `sandbox_driver` is docker-local only. This spec needs:
  - a boat agent launch;
  - the in-VM channel proxy.
- **Codex** runs only host-native today, with no sandbox. Until a sandboxed Codex launch exists, Codex agents run only on the owner's machines and never on VMs, and the waiting reason says so **(my call)**.

## 8. Constraints

- **Agent channel:** agents reach AQ only through the agent channel (spec 0006). No AQ secret enters a sandbox, on any machine.
- **No API keys:** subscription logins only. Credentials are never logged or put in argv.
- **Plaintext HTTP:** a node sends its credential over plaintext only to numeric loopback. A non-loopback server URL must be `https` (as `NativeClient` enforces).
- **Measurements:** a node's resource measurements expire after 24 h. A long-running service must re-measure on its own; today an expired measurement silently refuses every offer.
- **Build rules** (AGENTS.md): put new code in new crates, ratchets only go down, and tests go into existing binaries.
- **Contracts:** a server contract change merges together with the dashboard re-pin.

## 9. Left to the builder

- **Phasing.** A likely order is service install and start/stop, then the swarm page read-only, then the controls and placement, then boat automation. The order follows dependencies.
- **Where the VPS docker-local worker** (`aq-worker-cloud`, spec 0005) ranks in placement **(my call: after local machines, before starting a VM, with conservative limits that protect the server on the same host)**.
- **Data model:**
  - machines vs nodes;
  - live CPU and RAM reporting;
  - how independent provider caps and sparse provider-class overrides are stored and versioned;
  - how running agents map to authoritative routing classes (model-name guesses are insufficient for enforcing limits);
  - how "holding back" reasons are computed and exposed.
- **Migration:** how the existing B4 per-project replacement budgets and any remaining per-project pool settings migrate to the swarm controls.
- **Install script:** distribution and update path.
- **Boat:** VM image, region and size defaults.
- **Live updates:** how the page streams load and terminal data.

## 10. Delivery phases

Each phase has its own implementation plan in `docs/superpowers/plans/`. Phases are ordered by dependency; phases 1 and 2 can run in parallel.

| Phase | Work | Depends on |
| --- | --- | --- |
| 1. Workers as services | A multi-slot `aq-worker run` that actually takes docker-local work, a Linux/WSL2 systemd service, enrollment by install script and single-use code, start/stop/drain from the server, a fresh attempt from saved work after a restart, automatic re-measurement | — |
| 2. Swarm page (read-only) | Server swarm read model (machines, nodes, agents, live CPU/RAM including the WSL2 Windows probe, holding reasons), the dashboard tree page (React Flow and dagre), the properties pane and the agent terminal | — |
| 3. Controls and placement | provider cap sliders and burn estimates in the collapsible Swarm header; Max agents in existing Intelligence settings; VM maximums, per-machine limits and thresholds; replacement of per-project pool sizes; requirement-driven routing with slot admission (§7.1); waiting reasons; measured usage and supported burn estimates | 1, 2 |
| 4. Cloud VMs | The server starts one boat VM per attempt, the boat agent launch, the in-VM channel proxy, the Codex restriction, the cloud Access token | 1, 3 |
| 5. macOS | launchd service, multi-arch templates and `aqv2`, a proven Docker Desktop channel transport | 1 |

Phase 1 server: see `docs/implementation/0007-P1-server.md`.
