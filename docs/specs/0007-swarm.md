# 0007: The swarm (always-on workers, cloud VMs and swarm controls)

**Status:** Draft, from the owner's brainstorming session on 2026-11-07, with the builder-check answers folded in. Awaiting owner approval.
**Repositories:** aq-server, aq-worker, aq-dashboard.
**Mockup:** `aq-dashboard/.superpowers/brainstorm/2382938-1794083621/content/swarm-tree-v3.html` (React Flow plus dagre, the same libraries as v1's graph).

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
- **Graceful throttling near provider plan limits.** This is deferred to a later brainstorming session **(owner)**; §7.4 has the interim behaviour.
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

1. **Controls bar** at the top (§5.3).
2. **Swarm tree** below it, with a **properties pane** on the right (§5.4).

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

### 5.3 Controls

The controls are sliders **(owner)**:

| Control | What it sets |
| --- | --- |
| Agents per provider (Claude, Codex, …) | Target number of concurrently running agents for that provider |
| Mix by intelligence class (fast, standard, deep, …) | Share of agents per class. This drives burn rate. |
| Boat VMs | Shows the current VM ceiling, which is also the ceiling on cloud agents, since each VM runs one agent. The maximum is set in Settings **(owner)**. |
| Projected burn gauge | Read-only. Shows how fast the configured swarm uses each provider's usage window. |

- **Sliders are ceilings, not commands** **(my call)**. AQ runs agents only for ready work, up to these numbers.
- **Slider ranges are bounded by Settings maximums** **(owner)**.
- Changes take effect on the next placement decision. They never kill running agents. Lowering a slider lets running agents finish **(my call)**.

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

Settings gains a **Swarm** section **(owner: maximums live in Settings)**:

- **Provider maximums:** the upper bound of each provider slider.
- **Cloud:**
  - the maximum number of boat VMs, which is also the maximum number of cloud agents running at once;
  - the VM size.
- **Per machine:**
  - maximum agents **(owner)**;
  - CPU threshold and memory threshold, above which no new agents start there **(owner)**. On the WSL2 desktop these measure **Windows host load**, not only the WSL VM, so a game running on Windows counts. That needs a small Windows-side probe **(my call)**;
  - a display name.
- **Add machine:** shows the one-line install command with a single-use enrollment code **(my call)**.

## 7. Behaviour

### 7.1 Placement

When ready work exists and a provider is under its slider target, AQ places an agent:

1. **Which machines are eligible.** An online machine whose worker is running, that is under its agent maximum, and under its CPU and memory thresholds.
2. **Local machines first,** then running VMs.
3. **Overflow to the cloud.** If no machine is eligible and running VMs are under the VM maximum, AQ starts a new VM **(owner: automatic scaling)**.
4. **Otherwise the agent waits,** and the waiting branch shows the specific reason (§5.2).

The intelligence-class mix is applied when the router picks a class for each task: the mix becomes the target shares across running agents **(my call; how the mix interacts with the router's per-task class choice is for the builder to settle and document)**.

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
- The burn gauge shows each provider window's level where AQ has a source.
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
  - how slider targets are stored;
  - how "holding back" reasons are computed and exposed.
- **Migration:** how the existing B4 per-project replacement budgets and any remaining per-project pool settings migrate to the swarm controls.
- **Install script:** distribution and update path.
- **Boat:** VM image, region and size defaults.
- **Live updates:** how the page streams load and terminal data.
