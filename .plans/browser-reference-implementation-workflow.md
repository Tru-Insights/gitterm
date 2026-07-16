# Browser Source-to-Destination Implementation Target

**Status:** active post-MVP target
**Depends on:** GitTerm V4 browser-control phases 1–5 (TRU-81 through TRU-85)

## Product Outcome

From a Codex, Claude Code, or Pi session running inside GitTerm, the agent can
use a source-backed layout or application to implement a matching route in a
destination application. The result must use the authoritative design-system
components and variants, not merely imitate the rendered page with substitute
markup or one-off CSS. The main Portal application is the first concrete
acceptance destination, not a constraint on the capability.

The agent should be able to complete the source-inspect-implement-verify loop
while the user watches both browser targets and can immediately disconnect
browser control.

## Primary Scenario

The user has:

- a reference site containing HTML-oriented layouts and routes built from the
  approved component system;
- the reference site's source code;
- a Portal application where those routes must be implemented and wired to real
  application behavior;
- authenticated browser sessions where required.

This Portal scenario is the first acceptance case, not a browser-control
restriction. Named targets and paired evidence must also support arbitrary
source/target comparisons such as `before`/`after`, `design`/`implementation`,
or two independent applications.

## Three Independent Workflow Inputs

Treat these as separate authorities even when they live in the same repository:

1. **Layout source** — the route or page that defines the intended composition,
   hierarchy, responsive layout, and visible states.
2. **Design system** — the approved component packages, component source,
   documentation, tokens, variants, and usage constraints. This is authoritative
   for which building blocks may be used.
3. **Implementation target** — the application route that must reuse the
   approved design system while adding real data, behavior, permissions, and
   application integration.

The design system may be shared by the layout source and implementation target,
or it may be consumed through separate packages. Its identity and approved
version must be explicit. Browser evidence cannot prove design-system usage;
the agent must verify that from source imports and component composition.

## Capability And Skill Boundary

Keep the implementation split into three layers:

- **GitTerm capabilities:** persistent authenticated browser ownership, arbitrary
  named targets, target-aware CDP operations, bounded evidence, comparison UI,
  diagnostic sanitization, permissions, and stable MCP schemas.
- **Reusable agent skill:** discover the three workflow inputs, trace component
  provenance, create the route contract, choose target names/evidence labels and
  viewports, drive the inspect-edit-verify loop, and enforce completion checks.
- **Project facts:** repository roots, approved design-system packages and
  versions, build/test commands, allowed domains, routes, and intentional
  differences. Keep these in repository instructions or task configuration.

Do not implement a Portal-specific workflow engine in Rust. The browser and
evidence primitives must remain useful without the skill, and the skill must
orchestrate those generic primitives according to each repository's facts.

For a selected route, the agent must:

1. Read the reference route source and trace its component imports.
2. Produce a compact component contract covering component names, import
   sources, props, variants, nesting, slots, and relevant state assumptions.
3. Capture the rendered reference at agreed desktop and mobile viewports.
4. Inspect the corresponding Portal route at the same viewports.
5. Implement or update Portal using the approved components from the contract.
6. Reload Portal, compare it against the retained reference, and iterate.
7. Use DOM, console, and network evidence to diagnose remaining differences.
8. Leave paired evidence showing the reference and final Portal result.

## Source And Browser Responsibilities

Component identity cannot be proven from rendered HTML alone. A framework
component and hand-authored markup can produce the same DOM.

- **Reference source is authoritative for component identity.** Imports,
  component composition, props, and variants establish what Portal must reuse.
- **The rendered reference is authoritative for behavior and presentation.**
  Screenshots, layout measurements, computed styles, responsive behavior, and
  runtime diagnostics establish how the implementation must behave.
- **Portal source is authoritative for implementation correctness.** Final
  verification must show the approved imports and composition rather than only
  visual similarity.

## Route Component Contract

Before editing Portal, the agent should produce a route-specific contract in a
compact form such as:

```text
/dashboard
  PageShell                        @approved/ui
    PageHeader title="Dashboard"   @approved/ui
      Button variant="primary"     @approved/ui
    Grid columns={3}               @approved/ui
      MetricCard × 3               @approved/ui
    DataTable variant="compact"    @approved/ui
```

The contract should also record:

- reference route file and relevant supporting files;
- authoritative component package for each component;
- important props, variants, slots, and nesting;
- data that is static in the reference but must be wired in Portal;
- intentional Portal differences;
- target desktop and mobile viewports.

## Required Browser Capabilities

### 1. Named Browser Targets

- Open, list, focus, and close arbitrary named targets such as
  `reference`/`portal`, `design`/`implementation`, or `before`/`after`.
- Direct every operation to an explicit target.
- Keep console and network diagnostics isolated per target.
- Preserve both live pages while the agent edits and reloads Portal.

### 2. Paired Evidence

- Capture viewport and full-page PNG screenshots.
- Label captures by target, route, viewport, and purpose.
- Retain a small bounded in-memory evidence set rather than only the latest PNG.
- Present captures from any two targets side by side in GitTerm.
- Allow recapturing either target without replacing evidence retained for the
  other target.

Automated pixel scoring is not required for the first version. The model and
user can compare paired images directly.

### 3. Structural DOM Inspection

Provide a bounded structural outline containing:

- semantic containers and meaningful visible nodes;
- tag, role, ID, classes, text summary, and parent/child relationships;
- stable node references for targeted follow-up inspection;
- layout bounds for visible nodes;
- relevant accessibility properties.

Do not dump an unbounded raw DOM into the agent context.

### 4. Targeted Layout And Style Inspection

For a selected node or strict locator, return a bounded set of implementation
details:

- display, position, flex, and grid properties;
- width, height, bounding box, margin, padding, and gap;
- font family, size, weight, and line height;
- foreground, background, and border colors;
- borders, radius, shadows, opacity, and visibility;
- relevant attributes and a capped `outerHTML` excerpt.

The tool must not expose unrestricted JavaScript evaluation.

### 5. Console Inspection

- Capture log, info, warning, error, assertion, and uncaught-exception entries.
- Include timestamp, source URL, line and column, and bounded stack context when
  Chrome provides them.
- Support filtering by target, severity, text, and time/cursor.
- Support an explicit clear operation.
- Keep storage bounded and sanitize diagnostic URLs.

### 6. Network Inspection

- Capture recent request lifecycle entries with target, method, sanitized URL,
  resource type, status, timing, and failure details.
- Make missing fonts, images, scripts, and API failures easy to identify.
- Support filtering by target, status, type, text, and time/cursor.
- Allow an explicitly selected response-body preview only when needed, with
  content-type checks and strict byte limits.
- Never expose cookies, authorization headers, passwords, unrestricted storage,
  or complete sensitive request headers.

## Source Access Requirements

- The agent session must have read access to the reference source root.
- The agent session must have write access to the Portal root.
- When the repositories are separate, GitTerm should pass the reference root as
  an explicit additional read-only directory rather than granting broad
  filesystem access.
- The authoritative component packages must be identified in the task or
  repository instructions.
- Reference source must remain unchanged unless the user separately authorizes
  changes to it.

## Authentication And Permissions

- Authentication is completed by the user in GitTerm's persistent browser
  profile; agents must not request or handle passwords, MFA secrets, cookies,
  or authentication headers.
- A site/domain permission policy should restrict which authenticated sites an
  agent may inspect or control.
- New domains should require an observable user approval before agent access.
- The user must always have a visible activity indicator and immediate
  disconnect control.
- Evidence remains in memory by default and is not persisted automatically.

## Agent Workflow Contract

An agent performing this workflow must:

1. Inspect repository instructions and identify the authoritative component
   system before editing.
2. Read the reference implementation and create the route component contract.
3. Capture reference evidence before modifying Portal.
4. Prefer existing Portal patterns and approved component APIs.
5. Not replace an available approved component with custom markup or copied CSS.
6. Document any necessary divergence from the reference component contract.
7. Verify source imports and component composition after editing.
8. Verify desktop and mobile renderings, console state, and relevant network
   requests.
9. Iterate until material differences are resolved or explicitly reported.

## Delivery Slices

1. **Named targets and paired evidence**
   - target-aware controller and MCP tools;
   - viewport and full-page labeled captures;
   - bounded target-agnostic paired evidence viewer.
2. **DOM and computed-style inspection**
   - compact structural outline;
   - stable node references;
   - targeted layout/style details.
3. **Deep console and network diagnostics**
   - per-target event buffers, filtering, cursors, and clearing;
   - bounded selected response previews with sensitive-data safeguards.
4. **Source-backed implementation workflow**
   - explicit additional read-only reference root;
   - reusable route component-contract guidance;
   - code-level verification of approved component usage.
5. **Real-route acceptance and polish**
   - run the workflow on a representative reference and Portal route;
   - address usability, context-size, stability, and permission gaps discovered
     during the real implementation loop.

## Acceptance Target

Choose one real route that exercises layout, typography, shared components,
responsive behavior, and authenticated Portal data.

Acceptance has two gates:

1. **Capability gate:** GitTerm's generic tools can manage arbitrary named
   targets, route operations and diagnostics independently, retain bounded
   paired evidence, and compare captures without source/Portal assumptions.
2. **Workflow gate:** a fresh agent is given the reusable implementation skill
   plus a real task identifying the layout source, design-system authority, and
   implementation destination. It must complete the route without relying on
   hidden conversational context or Portal-specific browser behavior.

The acceptance artifacts therefore include the reusable skill, the concrete
source-to-destination task, the resulting source diff, and paired final browser
evidence.

The target is accepted when one agent session can:

- read both source trees with the intended read/write boundaries;
- identify and document the exact approved reference components;
- keep reference and Portal targets open concurrently;
- capture paired desktop and mobile baselines;
- implement Portal using the same component packages, variants, and composition;
- diagnose runtime differences through DOM, console, and network inspection;
- reload and iterate without losing the reference baseline;
- produce paired final evidence for user review;
- show no unexpected console errors or failed required resources;
- leave the reference source unchanged;
- complete the workflow without copying the rendered reference into substitute
  markup.

Visual similarity alone does not satisfy acceptance. Component provenance and
source composition must also be verified.

## Explicit Non-Goals For The First Target

- Editing the reference source.
- Unrestricted page JavaScript evaluation.
- Exposing browser cookies, credentials, authorization headers, or storage.
- Automatic video or GIF recording.
- Persistent evidence history across GitTerm launches.
- A universal numerical pixel-parity threshold.
- Attaching to the user's normal Chrome profile through an extension.
- Automatically wiring real Portal data solely from the static reference.

## Target Inputs To Record Before Implementation

- Reference source root and reference base URL.
- Portal source root and Portal base URL.
- First reference and Portal routes.
- Authoritative component package or packages.
- Design-system source or documentation root and approved version, when
  separate from the application repositories.
- Required authenticated domains.
- Desktop and mobile viewport dimensions.
- Intentional differences that should not be treated as parity failures.
