# Engineering Principles

Principles distilled from incidents and reviews on this codebase. Each is
referenced by ID (e.g. `RUST-1`) so reviewers and humans can call them out
unambiguously in PRs and commit messages.

Categories:
- `RUST-N` — Rust-specific patterns (types, validation, comments)
- `CONTROL-N` — Battery control and safety-critical logic

Add a new principle when:
- A bug, review comment, or incident reveals a class of mistake that's worth
  catching reflexively next time
- The lesson is general (applies beyond the one file that triggered it)
- It's behavioural ("do/don't do X") not factual ("X lives at path Y"); the
  latter belongs in a README

---

## RUST-1: Comments explain WHY, not WHAT

Document a function only when its name and types don't already make it obvious, and
then in one or two lines. Do **not** write historic decision records, rationale for
rejected alternatives, or statements about what the code deliberately does *not* do.
Never reference past versions or earlier implementations. A doc comment that wants
to be longer is evidence the function is doing too much — split it instead of
describing it.

**Why:** Code comments that explain WHAT restate what the types/names already say,
and comments that reference past versions ("an earlier version subtracted...") rot
as the codebase evolves. They bloat PRs (1000+ lines with 80% verbose comments) and
regenerate findings on every review round because prose is neither compiled nor
tested.

**How to apply:**
- Module-level docs orienting a reader are fine and encouraged. Per-function
  rationale is not.
- Explain a decision in the PR description or the commit message, where it is dated
  and doesn't pretend to describe current behaviour. Not in a comment that outlives
  the reasoning.
- If you're writing "this used to be X, which is how Y broke", stop. That belongs in
  git history.
- Only write a comment when:
  - A hidden constraint or subtle invariant isn't obvious from the code
  - A workaround for a specific bug exists
  - Behavior would surprise a reader given the types/names
  - The cost is low (one line, not a block)

---

## RUST-2: Physical quantities get newtypes

Never a bare `f64`/`i32`/`u32` for something with a unit or a role. The type system
must make confusion unrepresentable.

- **Units.** `Watts`, `Amps`, `MilliAmps`, `WattHours`, `Percent`, `Millis`. A
  charger setpoint in milliamps (Peblar) and one in whole amps (Vestel) are
  different types; mixing them is a 1000x error that reaches hardware.
- **Roles.** A signed flow (`grid_power`), a non-negative cap (`max_charge_power`)
  and a commanded setpoint (`power_watts`) are all watts and must be distinct types.
  Converting between them is an explicit call, never an implicit assignment.
- **Durations.** Never a bare count of seconds, minutes or millis — use `Duration`
  or a newtype that names the unit.
- **Validation belongs in the constructor.** `Soc::new` clamps once; call sites
  never re-check.
- **`#[serde(transparent)]`** so newtypes serialize as bare numbers. Wire formats
  (MQTT, HA discovery, the journal) remain unchanged.

**Why:** In embedded control systems reaching hardware, a 1000x unit error is
invisible to tests but catastrophic in production. Type safety makes these
unrepresentable.

**How to apply:**
- A cast (`as i32`, `as f64`) in the decision path is a smell: it means a quantity
  crossed a boundary without anyone saying what the conversion meant. Grep for and
  eliminate every cast.
- Before adding a bare number type, ask: "could this be confused with a different
  unit or role?" If yes, make a newtype.

---

## CONTROL-1: Validation at the boundary, not on every call

Validate user input, config, and external state at the point it enters the system.
Call sites inside the decision path assume their inputs are valid.

**Why:** Defensive programming that repeats the same check at every call site
masks the real invariant and costs clarity without catching bugs (if the input
passed once, the next check is cargo-culting). It also obscures what can actually
vary — in control loops, that distinction matters.

**How to apply:**
- Validate config at startup. Validate MQTT payloads at the subscription point.
  Validate device state on first read.
- Call sites inside the engine and controller assume inputs satisfy their type
  invariants.
- If you find yourself re-validating inside the loop, the invariant is wrong. Fix
  the type or the entry point, not the loop.
