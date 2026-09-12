# Cordis — core mechanisms, for a faithful Rust reimplementation

**Subject:** *Cordis*, the "Meta-Framework of Spatiotemporal Composability" from the DeepSeek-AI / `cordiverse` project.
**Paper:** *A Programming Paradigm for Spatiotemporal Composability*, Yifan Shi, Wei Zhang, Tianyi Cui (Peking University; DeepSeek-AI), arXiv:2608.25512v1 [cs.PL], 26 Aug 2026, 92 pages — [abs](https://arxiv.org/abs/2608.25512) · [pdf](https://arxiv.org/pdf/2608.25512v1) · [paper repo](https://github.com/cordiverse/paper).
**Implementation:** [`cordiverse/cordis`](https://github.com/cordiverse/cordis), cloned at commit `f8ea3cd` ("chore: bump versions"), npm `cordis@4.0.0-rc.10` + `@cordisjs/plugin-{loader,hmr,include,group}@1.0.0-rc.7/1.1.0/1.1.0/1.0.0`.
**Docs site:** [`cordis-primer`](https://deepseek-harness.github.io/deepseek-harness/reference/cordis-primer) (Chinese; official Cordis docs "still under construction" per the README). `cordis.js.org` does not resolve. The harness vendors the framework ([vendor README](https://github.com/deepseek-ai/deepseek-harness/blob/master/vendor/cordis/README.md)).

## 0. Provenance, method, and trust markers

| Marker | Meaning |
|---|---|
| **[P]** | Verified by reading the paper PDF text (arXiv:2608.25512v1). No HTML version exists (arXiv returns 404 for `/html/2608.25512v1`), so all paper quotes come from `pdftotext -layout` of the PDF. |
| **[C]** | Verified by reading the cordis source at commit `f8ea3cd`. |
| **[T]** | Verified by reading the repo's own vitest tests (executable specification). |
| **[I]** | Inferred by me; not directly stated by either source. |

Method: cloned the repo read-only into `/tmp/cordis_res/cordis`; read `packages/core/src/*.ts` in full, `packages/core/tests/*.spec.ts` for the effect/lifecycle/isolation/plugin semantics, `packages/loader/src/**`, `packages/hmr/src/index.ts`, `packages/include/src/*.ts`, `packages/{group,utils}/src/index.ts`, and all `package.json`s; extracted the 92-page PDF to text and read §3, §3.3, §3.4, §4.1–4.2, §4.4, §5, §6. Not read: `packages/logger-console`, `packages/timer`, `packages/create`, `packages/core/bin.js` (CLI bootstrap), and the bodies of `hmr/src/error.ts` beyond its exports.

**⚠ The paper describes an idealized runtime; the shipped code differs in naming and in several semantics.** Section 12 lists every divergence I found. A Rust port should pick one authority per mechanism; my recommendation is: **names from the code, semantics from the code, and the calculus from the paper as the correctness target.**

---

## 1. The core `Context` type: effect context and coeffect context unified

### 1.1 Theory [P]

The paper's unified context (§3.3.1, Definition 28) is a recursive triple:

```
Γ∞ ≔ μΓ. Γ × (Γ → Γ) × Σ
```

with three projections: **Γ** the current context state (recursive), **Γ → Γ** the *accumulator* that reverts this level's effects, and **Σ** the *coeffect context*. The coeffect context itself (§3.2.1, Definition 19) is a dependent partial function:

```
Σ ≔ (k : K) ⇀ V_k          -- finite partial map from key to typed value
```

with `σ(k)` application, `σ[k ↦ v]` extension, `σ \ k` restriction, `k ∈ dom(σ)` membership. Extension requires `k ∉ dom(σ)`, restriction requires `k ∈ dom(σ)`; a violated precondition *errors and produces no transition*, so the effect algebra still applies [P §3.2.1].

Crucially (§3.3.1): *"Since the type family V underlying Σ is unconstrained, any state the system needs to share across components can be encoded as a dependency with an appropriate value type — Σ subsumes all shared mutable states, not just inter-component dependencies. Every interaction between a component and its environment passes through this single entity."* The unification is therefore: **one object carries (state, revert-accumulator, dependency table), and every mutation of it is a tracked effect whose inverse lands in the accumulator.**

Each key carries more than a value type (Definition 29): a coeffect at `k` is a pair `(V_k, A_k)` where `A_k` is a set of **coeffect operations** the value publishes; an operation `a : X_a → V_k ⇀ V_k × (V_k ⇀ V_k) × B_a` — i.e. its first two constituents form an *effect function on the value*, its third an outcome. Its lift to Σ (eq. 31) reads/writes only the binding at `k`:

```
a_Σ(x)(σ) ≔ let (v, g, b) = a(x)(σ(k)) in (σ[k ↦ v], λσ'.σ'[k ↦ g(σ'(k))], b)
```

This is why "every interaction passes through the context" is a *discipline and not a property*: the discipline is Definition 30, which says a component's work is an **iterator whose every iteration is one of exactly three things** — an operation stage at a key in `d ∪ p`, a provision stage at a key in `p`, or an instantiation of another component. Anything else (reading a location no key binds) falls outside the calculus [P §3.3.1, §4.2.3 Definition 56].

### 1.2 Implementation [C]

`packages/core/src/context.ts` realizes Γ∞ as a **prototype-chained object graph plus a Proxy**, not as a literal triple. The three projections live in different places:

```ts
export interface Context {
  [symbols.isolate]: Dict<symbol>   // ρ : key → realm symbol      (part of Σ's addressing)
  [symbols.intercept]: Dict         // ι : key → metadata           (Σ_inter)
  root: this                        // the root context (a Proxy)
  baseUrl?: string
  events: EventsService
  logger: LoggerService
  reflect: ReflectService           // owns `store` (the value store σ) and `props` (key declarations)
  registry: RegistryService         // owns the plugin→runtime table (dom F_γ)
}
```

plus `ctx.fiber: Fiber` (declared by module augmentation in `fiber.ts`), which is where **the accumulator and the lifecycle state actually live**:

| Γ∞ projection | Runtime home |
|---|---|
| current state Γ | the object graph: `ctx` chain + `reflect.store` + `registry._internal` + everything effects touched |
| accumulator `Γ → Γ` | `fiber._disposables` (a `DisposableList<Disposable>`) |
| coeffect context Σ | `reflect.store` (values, keyed by **realm symbol**) + `reflect.props` (declarations) + `ctx[isolate]` (ρ) + `ctx[intercept]` (ι) |

Construction (`Context` constructor) creates the Proxy, then a root `Fiber`, then the four services; `this.fiber._disposables.clear()` removes the root fiber's bootstrap entries.

**Derived contexts.** The code's `extend()` is the paper's *derived realization* (Definition 23 [P §3.2.3]):

```ts
extend(meta = {}): this    // Object.create(getTraceable(this, this)) + defineProperty for meta's own keys
isolate(name: string, label?: symbol): this   // extend({[isolate]: shadow}) where shadow[name] = label ?? Symbol(name)
intercept(name: string, config: any): this    // extend({[intercept]: shadow})
```

Both `isolate` and `intercept` produce a *fresh derived context* whose table differs from the inherited one; the inverse is the identity and recovery is discarding the child. Nothing in the shared table changes, so there is nothing to track — exactly the paper's *derived realization* [P §3.2.3, §5.1.2: *"recovery is implicit: discarding the child context suffices, with no explicit inverse to run"*].

**Symbol key registry** (`utils.ts`, identical to the paper's `@@name` notation): `shadow`, `caller`, `receiver`, `original`, `metadata`, `initHooks`, `checkProto`, and the four context symbols `effect`, `filter`, `isolate`, `intercept`; plus service symbols `init`, `check`, `config`, `invoke`, `extend`, `tracker`, `resolveConfig`. All are `Symbol.for('cordis.*')`, i.e. process-global.

**Service surface on every context** (`ReflectService` constructor): `mixin('reflect', ['get','set','provide','accessor','mixin'])`, `mixin('fiber', ['runtime','effect'])`, `mixin('registry', ['inject','plugin'])`, `mixin('events', ['on','once','parallel','emit','serial','bail','waterfall'])`. So `ctx.on`, `ctx.effect`, `ctx.plugin`, `ctx.get` are *derived accessors* onto the four services, resolvable through the same Proxy.

**Def-site / use-site.** A non-obvious but load-bearing mechanism: a value carrying `symbols.tracker` is wrapped by `createTraceable(ctx, value, tracker)` into a Proxy that records a **def site** (where the accessing code was defined; governs service resolution) and a **use site** (where the service is consumed; governs intercept, isolate and effects). `ctx[symbols.shadow]` holds the def site when `ctx` is itself a shadow pair. This is how `ctx.foo.bar()` can resolve `foo` through the *declaring* module's inject while attributing the effect to the *using* fiber. `getTraceable` is applied at every service read (`reflect.get`, the Proxy get trap's `impl.value` return).

**Rust mapping note.** The recursive `μΓ` should be modeled as an explicit parent pointer, not as JS prototype chains: a `Context` value holding `parent: Option<Arc<Context>>` (or a `&Context` in an arena), an `isolate: Arc<HashMap<Key, Realm>>` copied-on-write by `isolate()`, an `intercept: …` likewise, and a `fiber: FiberId`. The def-site/use-site split has no lazy equivalent in Rust — you must pass the context explicitly where JS lets `ctx.foo` resolve implicitly, so the *discipline* ("all access goes through a context, and the context you hold determines what you may see") becomes a compile-time property rather than a Proxy trick (the paper anticipates exactly this at §6.4 [P]).

---

## 2. Effect tracking and revertible effects

### 2.1 Theory [P §3.1]

* **Effect context** (Definition 2): `∂Γ ≔ Γ × (Γ → Γ)` — a state paired with an *accumulator* 𝜑, "the composite of the inverses of the effects performed so far". Initial state `(γ₀, id_Γ)`. `∂²Γ = ∂(∂Γ)`, giving a tower.
* **track** (Definition 3): `track_Γ(𝑓, 𝑔) = (γ, 𝜑) ↦ (𝑓(γ), 𝜑 ∘ 𝑔)`. Theorem 4: tracking leaves forward behaviour untouched (`pr1 ∘ f' = f ∘ pr1`). Theorem 5: `track_Γ` is a monoid homomorphism from the *twisted composition* monoid 𝔗Γ (Definition 1: `(f₁,g₁) ∘ (f₂,g₂) ≔ (f₁∘f₂, g₂∘g₁)`) into `∂Γ → ∂Γ`.
* **Soundness invariant**: `𝜑(γ) = γ₀`. Theorem 7: for any pair with `g(f(γ)) = γ₀`-correctness at the applied state, `recover_Γ(track_Γ(f,g)(γ,𝜑)) = recover_Γ(γ,𝜑)`, where `recover_Γ = (γ,𝜑) ↦ (𝜑(γ), id_Γ)`.
* **Effect function** (Definition 8): `𝔈_Γ ≔ Γ → Γ × (Γ → Γ)`, **witnessed** as `𝔈*_Γ` by an extra component `(γ)(δ)(g). (δ,g) = e(γ) → g(δ) = γ` — *the inverse is required to revert only at the state where it was applied*, so it may differ per state.
* **Effect composition** (Definition 9): `f ⋄ g = γ ↦ let (δ,s) = g(γ) in let (ε,t) = f(δ) in (ε, s ∘ t)`. Inverses accumulate in reverse order. Theorem 10: (𝔈_Γ, ⋄) is a monoid with unit `η_Γ = γ ↦ (γ, id_Γ)`. Theorem 11: witnessing survives `⋄`, and a *uniform* inverse `g ∘ f = id_Γ` witnesses at every state.
* **Lifting** (Definition 12): `effect_Γ(e) = (γ,𝜑) ↦ ((δ, 𝜑∘g), track_Γ(g, pr1∘e))`. Theorem 14: the lifted forward map projects onto the unlifted one. Theorem 15: `g'(Δ) = (γ, 𝜑 ∘ g ∘ f)`; the *state* is recovered exactly always, the *accumulator* is restored iff `g ∘ f = id_Γ`; in every case `(𝜑 ∘ g ∘ f)(γ) = 𝜑(γ)`, so the soundness invariant survives. **Note**: `effect_Γ` does *not* carry `𝔈*_Γ` into `𝔈*_{∂Γ}` — that is why the relaxation to observational equivalence (§9) is needed.
* **Effect iterator** (Definition 17): `ℑ_Γ ≔ μℑ. Γ → Γ × (Γ → Γ) × Maybe(ℑ)` — each iteration yields the new state, an inverse, and a continuation (`Nothing` = terminate). **Definition 18** `effectiter_Γ` recurses: `Nothing ⇒ ((δ, 𝜑∘g), track_Γ(g, pr1∘i))`; `Just(i') ⇒ let (s,r) = effectiter_Γ(i')(δ, 𝜑∘g) in (s, t ∘ r)`. Each iteration's inverse is appended, so **the accumulator reverts in LIFO order** (Theorem 16: reverting in reverse order hands each inverse the state its own application produced, and every intermediate state satisfies the soundness invariant). A plain effect function embeds as the one-iteration iterator.
* The paper's operational summary: *"Loading a component is running one iterator and accumulating its inverses in 𝜑; unloading it is applying 𝜑."*

### 2.2 Implementation [C]

The single mutation primitive is **`ctx.effect(callback, label?)`** — the realization of `effectiter_Γ` (§5.1.1 [P]: *"Every context mutation in Cordis flows through a single primitive, `ctx.effect`: coeffect provision, component instantiation, and every other context-mutating operation reduces to a `ctx.effect` call"*).

**Exact signature** (`fiber.ts`):

```ts
effect(execute: () => SyncEffect,  label?: string): Disposable<Promise<void>>
effect(execute: () => Effect,      label?: string): AsyncDisposable<Promise<void>>

type Disposable<T = any> = () => T
type SyncEffect<T>  = Disposable<T> | Iterable<Disposable<T>, void, void>
type Effect<T>      = SyncEffect<T> | Promise<Disposable<T>> | AsyncIterable<Disposable<T>, void, void>
```

**Accepted callback shapes** — `_execute` dispatches adversarially [C], and non-conforming returns throw `TypeError('Invalid effect')`:
1. a function → that function *is* the inverse (degenerate iterator, eq. 19);
2. `null`/`undefined` → no-op, no inverse;
3. a thenable → `effect.then(safeCollect)` (async: the resolved function is collected when it arrives);
4. a sync iterable → drive to completion **eagerly**, collecting every yielded function;
5. an async iterable → drive at the first `await Promise.resolve()` suspension, **checking `runner.epoch !== oldEpoch` before each `iter.next()`** (this is the step-boundary interruption of §4.2.2, i.e. `L-Divert`);
6. anything else → `TypeError('Invalid effect')`.

**Where the disposers live.** Two levels, and the distinction matters for exactness:

*Per-call*: `Fiber.effect` allocates a local `const disposables: Disposable[] = []`, and the returned wrapper's `dispose()` does

```ts
let task!: void | Promise<void>
for (const dispose of disposables.splice(0).reverse()) {
  if (task) task = task.then(dispose)      // strictly sequential, never concurrent
  else { const result = dispose(); if (isObject(result) && 'then' in result) task = result }
}
return task
```

so **within one `ctx.effect` call the inverses run in strict LIFO order, sequentially awaited**.

*Per-fiber*: the wrapper itself is pushed onto `this._disposables` (a `DisposableList<Disposable>`, `utils.ts`: `Map<serial, T>` + `WeakMap<T, serial>`; `push` returns a remover closure; **`clear()` returns `values.reverse()`**). This is the ∂²Γ structure — *"a child effect's inverse is itself an effect on the parent"* (§5.1.1 [P]).

**Revert ordering across a fiber.** `Fiber._unload()` [C] is:

```ts
await Promise.all(this._disposables.clear().map(async (dispose) => {
  try { await composeError(async (info) => { await Promise.resolve(); info.error = new Error(); await dispose() }, this._runner.getOuterStack) }
  catch (reason) { this.ctx.logger.error(reason) }
}))
```

So **sibling top-level effects of one fiber are initiated concurrently** (`Promise.all`), not in LIFO order. The paper acknowledges this at §5.1.3: *"the wait sits ahead of the whole recovery rather than inside one of the inverses being waited on, since `fiber.dispose` initiates a fiber's effects concurrently and a wait placed within one of them would leave the rest unordered."* **The LIFO guarantee is therefore per-`ctx.effect`-call, not per-fiber.** A caller that needs ordered teardown across several resources must put them in *one* effect (the docs site says exactly this: *"If teardown order is required, put the related work in the same effect"* [docs]).

**Self-disposal / idempotence.** The wrapper closes over `runner.epoch: boolean`; `dispose()` returns immediately if `epoch` is already false, and sets it false before doing work. This is both "recovery fires at most once" and "the guard halts any in-flight iteration" (§5.1.1 [P]). Verified by test [T `dispose.spec.ts`]: double `fiber.dispose()` / double `dispose()` runs each disposer exactly once.

**Error handling.**

| Situation | Behaviour | Evidence |
|---|---|---|
| Callback throws synchronously | already-collected inverses are reverted (`dispose()`), then the error is **rethrown to the caller** | [C] `try { task = this._execute(runner) } catch (reason) { dispose(); throw reason }`; [T] "return with error" → `seq == []`; "yield with error" (throws after 1 yield) → `seq == [1]` |
| Async callback rejects | `task?.catch(dispose).catch(err => ctx.logger.error(err))` — revert collected inverses, then **log**; the returned thenable also rejects to an awaiter of the wrapper | [T] "async return with error" → `await expect(dispose).rejects.toThrow()`, `seq == []`; "async yield with error" → rejects, `seq == [1]` |
| A disposer throws during unload | caught, **logged via `ctx.logger.error`**, never propagated; the remaining disposers still run | [C] `_unload`'s catch; [T] "dispose error" → `await fiber.dispose()` resolves `undefined`, error logged once |
| Callback runs on an inactive fiber | `Fiber.assertActive()` throws `CordisError('INACTIVE_EFFECT')` = `"cannot create effect on inactive context"` | [C] `assertActive`; [T] "inactive context" checks `ctx.plugin`, `ctx.effect`, `ctx.on` all throw inside a disposed fiber's inverse |

**Stack-trace handling.** `composeError(callback, getOuterStack)` (`utils.ts`) wraps every effect body and every inverse. It splices the *outer* call site's stack lines (captured by `buildOuterStack()`, or by the loader's `Entry.getOuterStack` which yields `    at <baseUrl>#<id>` frames) into the thrown error's stack, so an error inside a plugin's effect points at the plugin entry that installed it.

**Introspection.** `defineProperty(wrapper, symbols.effect, meta)` where `meta: EffectMeta = { label, children: EffectMeta[] }`; labels come from the call site (`'ctx.on("x")'`, `'ctx.provide("x")'`, `'ctx.plugin()'`, `'ctx.isolate("x")'`, `'ctx.mixin(...)'`) or the explicit `label`. `fiber.getEffects()` returns the top-level metas with nested children. Exact expected shape is asserted in [T `dispose.spec.ts` "yield dispose"].

**Verified async-abort semantics** [T `dispose.spec.ts` "async yield 2/3"]: disposing while an iteration is suspended does **not** cancel the body in flight; the loop stops at the *next* iteration boundary, and an iteration that already ran to completion has its inverse collected and reverted (test 3 yields `[1, 3, 4, 2]` — the second iteration's body ran and pushed 3, its inverse 4 ran, then inverse 2). **This is inertia** (§4.4 [P]: an asynchronous host *"is inertial: of L-Divert it takes the landing alternative alone"*).

**Confinement (the correctness obligation the runtime does *not* check).** §5.1.1 [P]: *"What the operation does not check is the witness that 𝔈*_Γ carries: the callback supplies an inverse, and that the inverse reverts the effect it accompanies is an obligation on the component author rather than a property the runtime verifies."* The paper's Definition 55 makes the obligation precise and is the right thing to port as a documented contract: a map `f` is **confined to `n`** when (1) *Writes* — it changes no fiber's presence, changes other fibers only in `σ_m|_{d_n}`, and itself only in `σ_n`; (2) *Reads* — it distinguishes states only by `σ_n` and `σ_m|_{d_n}`.

---

## 3. Reactive coeffects: declaration, activation, deactivation

### 3.1 Theory [P §3.2]

* **get / set** (Definition 20): `get = k ↦ σ ↦ σ(k)` (requires `k ∈ dom σ`); `set = (k,v) ↦ σ ↦ (σ[k↦v], λσ'. σ' \ k)` (requires `k ∉ dom σ`). **`set(k,v)` has type `𝔈*_Σ`** — a coeffect provision *is* an effect function, hence revertible. The paper calls this *"the synergy between reactive coeffects and revertible effects: coeffect operations are effects, and effects are revertible."*
* **Satisfaction** (eq. 22): `σ ⊧ d ≔ ∀k ∈ d. k ∈ dom(σ)`, decidable because `dom(σ)` is finite. **Coeffect specification** (Definition 21): `𝔇_Σ ≔ Set(K)`.
* **Notification, the reactivity rule** (Definition 22): for a specification `d` and transition `σ → σ'`:
  `notify_d(σ,σ') = activating if σ ⊭ d ∧ σ' ⊧ d; deactivating if σ ⊧ d ∧ σ' ⊭ d; neutral otherwise`.
  *"An activating transition triggers the execution of the component's effects... and a deactivating transition triggers recovery by applying the accumulator."*
* **The algebraic basis of reactivity**: *"Since all mutations to σ pass through effect functions (whose inverses recover the previous domain), changes to satisfaction are detectable at each effect boundary."* [P §3.2.2]
* **Local spatial composability** (§3.2.2): *"a component activates only at a state satisfying its specification, so it never reads a binding that is absent, and every change to the context is classified against that specification."* The paper names exactly what is left out and deferred to the global theory: (a) withdrawing a binding only after the deactivations it causes have finished, and (b) keeping the bindings an activation reads unmoved while it runs. Both become Theorem 70 in §4.3.3.
* **Isolation** (Definitions 24–25): `Σ_iso ≔ (K ⇀ R) × ((r:R) ⇀ V_r)`, i.e. `(ρ, σ)`; `get(k) = σ(ρ(k))`; `set(k,v) = (ρ, σ[ρ(k)↦v])` with inverse `σ' \ ρ'(k)`; `isolate(k, r) = (ρ[k↦r], σ)` — a *map from context to context*, not an effect function, because it changes nothing shared ("a key already isolated is reassigned rather than refused"). Keys outside `dom(ρ)` resolve to their own realm (`ρ(k) = k`). The paper calls this *"a runtime ad-hoc polymorphism system"*.
* **Interception** (Definitions 26–27): `Σ_inter ≔ ((k:K) → M_k) × ((k:K) ⇀ (M_k → V_k))` as `(ι, σ)`; `get(k, μ) = σ(k)(μ ⊕_k ι(k))`; `intercept(k, ν) = (ι[k ↦ ι(k) ⊕_k ν], σ)`, again a derived map. Each key's metadata carries a monoid `(M_k, ⊕_k, ε_k)`; **the merge is right-biased so `ι(k)` (context-carried) takes priority over the component's declaration**, letting an enclosing context constrain a component without modifying it.

### 3.2 Implementation: how dependencies are declared [C]

**Static declaration on the plugin object** (`registry.ts`):

```ts
type Inject<M = Dict> = (keyof M)[] | { [K in keyof M]?: M[K] }        // array form or map form
type InjectKey = keyof { [K in keyof Context & string as Context[K] extends {[symbols.config]: any} ? K : never]: any }

interface Plugin.Base<T> { name?: string; Config?: StandardSchemaV1<any,T>; inject?: Inject; provide?: string|string[]; intercept?: Dict<boolean> }
Plugin.Function<T>    = Base<T> & ((ctx: Context, config: T) => any)
Plugin.Constructor<T> = Base<T> & (new (ctx: Context, config: T) => any)
Plugin.Object<T>      = Base<T> & { apply(ctx: Context, config: T): any }
```

So `inject: ['database']` or `inject: { database: {…} }` (map form supplies per-dependency *interception config*). `Inject.resolve(inject)` flattens to `Dict` (name → config-or-null), and specifically understands the `@Inject` class-decorator's prototype-chained `inject` object via `symbols.checkProto`.

**Dynamic / ad-hoc declaration.** Context methods (module augmentation in `registry.ts`):

```ts
inject(deps: Inject, callback: Plugin.Function<void>): Fiber & PromiseLike<Fiber>
plugin<P extends Plugin>(plugin: P, ...args: Spread<GetPluginConfig<P>>): Fiber & PromiseLike<Fiber>
```

`RegistryService.inject(inject, callback)` is literally `this.plugin({ inject, apply: callback, name: callback.name })`. Both return a `Fiber` wrapped in `Object.create(fiber)` with a `then` that calls `fiber.await()`, so `await ctx.plugin(P, cfg)` yields the settled fiber.

> **Naming correction:** the paper's `ctx.use` (§5.1.3 Algorithm 4, Table 2) is **`ctx.plugin()`** in the code. `ctx.using` does **not** exist anywhere in this revision (grep for `using` over the whole repo returns nothing); it is a Cordis-v3/Koishi-era API. `ctx.inject` is the code's sugar for `ctx.plugin({inject, apply})`.

**`@Inject()` decorator** (also `registry.ts`): `@Inject(name, config?)` on a class adds `value.inject[name] = config` with a prototype-chained `inject` object marked `symbols.checkProto`; on a method it records into `symbols.metadata.inject` and pushes an initializer that runs `ctx.inject(inject, ctx => value.call(withProps(this, {[tracker.property]: ctx})))` from `symbols.initHooks`, rebinding the tracker property to the injected context.

### 3.3 Implementation: the activation decision [C][T]

The decision machine lives entirely in `Fiber`:

```ts
export const enum FiberState { PENDING, LOADING, ACTIVE, FAILED, DISPOSED, UNLOADING }

class Fiber {
  uid: number | null                 // n : 𝔑   (registry.counter, monotonic, never reused)
  inject: Dict<any>                  // d : 𝔇Γ   (resolved spec)
  config: any
  state: FiberState
  store: Dict<Impl> | undefined      // ω : committed view, name → providing Impl
  inertia: Promise<void> | undefined // handle of the in-flight transition
  _disposables: DisposableList<Disposable>   // the accumulator g
  private _error: any
  private _runner: EffectRunner<string>      // epoch : string | '__INACTIVE__'
  private _store: Dict<Impl>                 // the freshly computed view
}
```

* **Resolution of one declared key** — `Fiber._checkImpl(name)`:
  `impl = ctx.reflect._getImpl(name, /*strict*/ true)`; strict means **the providing fiber must be `FiberState.ACTIVE`** — the runtime form of `provider_k(γ)`; then, if the impl carries a `check`, call `check.call(getTraceable(ctx, impl.value))` and drop the impl if it returns falsy. So a provider can *veto its own visibility dynamically* (the `Loader` uses this: `[Service.check]() { if (config.await && this.getTasks().length) return false; return true }`).
* **Target view digest** — `Fiber._refresh()`:
  ```ts
  let epoch: string | boolean = ''
  for (const name of Object.keys(this.inject)) {
    const impl = this._store[name]
    if (!impl) { epoch = INACTIVE; break }        // INACTIVE = '__INACTIVE__'
    epoch += ':' + impl.fiber.uid
  }
  this._setEpoch(epoch)
  ```
  The epoch string **is** `target_n(γ)` (Definition 53) hashed: a tuple of the *uids of the providing fibers*. **Recording the provider identity rather than the value is what makes the comparison correct** — "a uid is drawn fresh and never reused, so a provider that is replaced cannot be mistaken for the one it replaced, even when the two provide equal values" [P §5.1.3]. Consequence [P]: *"A provider that overwrites its own binding in place is therefore not observed; a component that wants its replacement to propagate withdraws the binding and installs it afresh."*
* **Transition initiation** — `Fiber._setEpoch(epoch)`:
  ```ts
  if (epoch === oldEpoch) return                       // neutral: notify_d's third case
  if (this._error) return                              // a FAILED fiber never re-enters
  this._runner.epoch = epoch
  if (this.inertia) return                             // inertia lock: one transition at a time
  this._updateState(() => epoch !== INACTIVE && oldEpoch === INACTIVE
    ? (this.inertia = this._reload(),  FiberState.LOADING)
    : (this.inertia = this._unload(),  FiberState.UNLOADING))
  ```
  So: activating iff `epoch ≠ INACTIVE ∧ old = INACTIVE`; deactivating otherwise (including provider replacement, which is `epoch` change → `_unload` → `_reload`). This is `notify_d`'s classification, with "neutral" collapsed into "epoch unchanged".
* **State read** — `Fiber._getState()`: `DISPOSED` if `uid === null`; `FAILED` if `_error`; `ACTIVE` if `epoch !== INACTIVE`; else `PENDING`. Thus **PENDING ≙ the paper's 𝖨𝗇𝖺𝖼𝗍𝗂𝗏𝖾** and `LOADING ≙ 𝖱𝖾𝗅𝗈𝖺𝖽𝗂𝗇𝗀`.
* **State-change side effects** — `Fiber._updateState`: emit `'internal/status'(fiber, oldState)` on any change; then **only when the change crosses the ACTIVE boundary**, call `ctx.reflect.notify([impl.name])` for every impl this fiber itself provides. That is `L-Leave`/`L-Finish` propagating the provider's availability change outward.

### 3.4 Implementation: the propagation loop [C]

**`ReflectService.notify(names, filter)` is the whole reactivity engine:**

```ts
notify(names: string[], filter = (ctx, name) => ctx[isolate][name] === this.ctx[isolate][name]): Fiber[] {
  const fibers: Fiber[] = []
  for (const runtime of this.ctx.registry.values())
    for (const fiber of runtime.fibers) {
      let hasUpdate = false
      for (const name of names) {
        if (!(name in fiber.inject)) continue          // only declared keys are re-evaluated
        if (!filter(fiber.ctx, name)) continue         // realm gate
        hasUpdate = true
        fiber._checkImpl(name)                          // recompute satisfaction for this key
      }
      if (!hasUpdate) continue
      fiber._refresh()                                  // recompute the target digest
      fibers.push(fiber)
    }
  for (const name of names) {                           // observable notification
    const self: Context = Object.create(this.ctx)
    self[symbols.filter] = (target) => filter(target, name)
    this.ctx.events.emit(self, 'internal/service', name, this._getImpl(name, false)?.value)
  }
  return fibers                                        // so callers can await the affected fibers
}
```

**Called from** four places: (1) `reflect.provide` — on install if the providing fiber is already `ACTIVE`, and on withdrawal; (2) `Fiber._updateState` — a fiber's own provisions on an ACTIVE-boundary crossing; (3) the loader's isolate plugin (`ctx.reflect.notify(Object.keys(diff), predicate)` after a realm reassignment); (4) `Entry.init()` (`ctx.reflect.notify(['loader'])` once the loader's tasks settle). The `internal/service` emit is realm-filtered via `symbols.filter`, so listeners registered in another isolation realm do not observe it.

**Idempotence makes neutral changes harmless** [P §5.1.2]. Verified [T `isolate.spec.ts`]: creating a second isolated context for a key that is already satisfied does not re-run the consumer; withdrawing the outer provider deactivates exactly the consumer that resolved it.

### 3.5 Value access: `ctx.get` vs the Proxy [C]

| | `ctx.get(name, strict=true)` | `ctx.foo` (Proxy get trap) |
|---|---|---|
| resolves against | `reflect.store[ctx[isolate][name]]` — the global table by realm | walks **up the fiber chain** over each fiber's *committed* `store` |
| missing key | returns `undefined` | throws |
| undeclared key | returns the value if provided | throws `cannot get property "foo" without inject` |
| declared but not committed | — | throws `cannot get required service "foo" in inactive context` |
| realm boundary | — | stops with the plain error when `fiber.parent[isolate][prop] !== key` |

The Proxy fallback (paper's Algorithm 6) is:

```ts
const key = target[symbols.isolate][prop]
let fiber = defSite.fiber
while (true) {
  const impl = fiber.store?.[prop]
  if (impl) return getTraceable(ctx, impl.value)
  if (prop in fiber.inject) { error.message = `cannot get required service "${prop}" in inactive context`; throw error }
  if (!fiber.runtime) throw error
  if (fiber.parent[symbols.isolate][prop] !== key) throw error
  fiber = fiber.parent.fiber
}
```

This is *the* enforcement point of the coeffect specification `d` at point of use, and it is *what makes a dependency readable during its own teardown* — the walk reads the committed view ω, which `L-Unload` discards only as its last act (Theorem 70; §5.1.4 [P]). Both the get and set traps first run a `waterfall('internal/get' | 'internal/set', …)` so an interceptor can wrap resolution.

**Write path.** `ctx.foo = v` → `internal/set` waterfall → `ReflectService.set(name, value)`:

```ts
const key = ctx[isolate][name]; const impl = store[key]
if (!impl) throw new Error(`cannot set property "${name}" without provide`)
if (impl.fiber !== ctx.fiber) throw new Error(`cannot set property "${name}" in multiple fibers`)
impl.value = value; return true
```

**No `notify`.** A value-only update is invisible to dependents (verified by reading; this matches §5.1.3 [P] quoted in §3.3). **This is the single most important discrepancy to get right in a port** — see §12.

---

## 4. Service registry semantics

### 4.1 The two-and-a-half APIs [C]

1. **Reflective**, on `ReflectService` (re-exported onto every context via `mixin`):
   ```ts
   get<K>(name: K, strict?: boolean): undefined | this[K]     // never throws for absence
   set<K>(name: K, value: undefined | this[K]): void           // same-fiber, in-place, silent
   provide<K>(name: K, value: undefined | this[K]): () => void // declares + installs + notifies — an EFFECT
   accessor(name: string, options: Omit<Property.Accessor,'type'>): void
   mixin<K>(name: K, mixins: (keyof this & keyof this[K])[] | Dict<string>): void
   mixin<T extends {}>(source: T, mixins: (keyof this & keyof T)[] | Dict<string>): void
   ```
2. **Property access** via the Proxy (`ctx.foo`, `ctx.foo = v`, `'foo' in ctx`).
3. **OO**, `abstract class Service<out T = never>` — the recommended form. `class Foo extends Service { constructor(ctx, config) { super(ctx, 'foo') } }`. The constructor ends with `self.ctx.reflect.provide(name, self, this[symbols.check])`, so a Service instance **auto-provides itself**: "a plugin is an object that implements a Service".

### 4.2 `ReflectService` state and the `Impl` record [C]

```ts
interface Impl { name: string; fiber: Fiber; value?: any; check?: () => boolean }
store: Dict<Impl, symbol>       // keyed by REALM SYMBOL, not by name   — this is σ
props: Dict<Property>           // declared properties: {type:'service'} | {type:'accessor', get, set}
```

`Property` is `{type:'service'}` or `{type:'accessor', get(this, receiver, error), set?}`. `props` is why `'foo' in ctx` works before anything is provided, and why a name declared as an accessor cannot later be provided as a service. Note that **`store` and `props` are owned by a single `ReflectService` instance reached from the root context** — they are process-global tables keyed by realm symbols; per-context behaviour comes from `ctx[isolate]` (which realm symbol this context maps a name to), not from separate stores.

### 4.3 `provide` — the tracked registration [C]

```ts
provide(name, value?, check?) {
  return this.ctx.fiber.effect(() => {
    if (!this.props[name]) this.props[name] ??= { type: 'service' }
    else if (this.props[name].type !== 'service') throw new Error(`property "${name}" is already declared as ${this.props[name].type}`)
    this.props[name] = { type: 'service' }
    this.ctx.root[symbols.isolate][name] ??= Symbol(name)      // the default realm: ρ(k) = k
    const key = this.ctx[symbols.isolate][name]
    const impl: Impl = { name, value, fiber: this.ctx.fiber, check }
    if (this.store[key]) throw new Error(`service "${name}" has been registered at <${this.store[key].fiber.name}>`)
    this.store[key] = impl
    this.ctx.fiber.store![name] = impl                          // make it visible to itself immediately
    if (this.ctx.fiber.state === FiberState.ACTIVE) this.notify([name])
    return async () => {
      delete this.store[key]
      const fibers = this.notify([name])
      await Promise.allSettled(fibers.map(fiber => fiber.await()))   // ← the guard, client side
      delete this.ctx.fiber.store![name]                             // "ensure self access before dependencies cleanup"
    }
  }, `ctx.provide(${JSON.stringify(name)})`)
}
```

Points to port exactly:
* **Single provider per key per realm**, enforced by throw — the runtime form of O-Insert's last premise `∀m ∈ dom(F_γ). p ∩ p_m = ⌀` [P §4.2.1]. Multiple implementations of one interface require *realms* or a *broker* (§6.2 [P]).
* **Withdrawal waits for dependents.** The disposer deletes the binding from the shared store, notifies, and **awaits every affected fiber to reach a settled state** before removing its own self-view. This is the code's rendering of the `¬relied_n(γ)` guard on `L-Unload` and of the ordering Theorem 70.
* **Reentrancy detail:** the fiber's own `store[name]` is set *before* notify and deleted *after* dependents settle, so a teardown that still reads its own provision works.
* `check` is the inject-checker: `Fiber._checkImpl` calls it, and a falsy result removes the impl from the *consuming* fiber's view without withdrawing the provision.

### 4.4 Isolation (scoping) [C][T]

* `ctx.isolate(name, label?)` derives a context whose `symbols.isolate` is `Object.create(parent[isolate])` with `shadow[name] = label ?? Symbol(name)`.
* Resolution is always `reflect.store[ctx[isolate][name]]`. Therefore the **same key resolves to independent bindings in different realms**, and two contexts that pass the *same explicit label* share one binding. `provide` uses `ctx.root[isolate][name] ??= Symbol(name)` for the default realm, so a key not isolated anywhere resolves through one root symbol.
* `Service[symbols.filter](ctx)` is the realm test: `ctx[isolate][this.name] === this.ctx[isolate][this.name]`. It is used by `notify`'s default filter and by event dispatch (`thisArg[Context.filter]`), which is why an isolated service only observes events emitted from its own realm. Verified [T `isolate.spec.ts` "isolated context" / "shared label" / "isolated event"].
* Isolation is **derived**, hence *not an effect*: no inverse is tracked; discarding the derived context is the recovery (Definition 23/25 [P]).

### 4.5 Interception [C]

`ctx.intercept(name, config)` derives a context carrying metadata. Two consumers exist in the kernel:
* `Service[symbols.resolveConfig](base?, head?)` walks the intercept prototype chain collecting `intercept[name]` entries, unshifts `base`, pushes `head`, and merges: `this['Config'].merge ? Config.merge(...configs) : Object.assign({}, ...configs)`. Right-biased: the innermost (closest to the use site) config wins, matching Definition 27's `μ ⊕_k ι(k)` with `ι` taking priority [P].
* `logger.spec.ts` shows `ctx.intercept('logger', {name: 'intercepted'})` overriding a service's config without touching the provider [T].

Interception is consulted at access time and needs no reload; changing it therefore does not perturb the dependency graph [P §5.2.1, §6.3].

### 4.6 `Service` base class [C]

```ts
abstract class Service<out T = never> {
  static readonly init/check/config/invoke/extend/tracker/resolveConfig: unique symbol   // = symbols.*
  declare [symbols.config]: T      // the service's own config type
  public name!: string
  constructor(protected ctx: Context, name: string) {
    name ??= this.constructor['provide'] as string
    let self = this
    const tracker: Tracker = { associate: name, property: 'ctx' }
    if (self[symbols.invoke]) self = createCallable(name, joinPrototype(Object.getPrototypeOf(this), Function.prototype), tracker)
    self.ctx = ctx; self.name = name
    defineProperty(self, symbols.tracker, tracker)
    self.ctx.reflect.provide(name, self, this[symbols.check])
    return self
  }
  protected [symbols.filter](ctx: Context) { return ctx[isolate][this.name] === this.ctx[isolate][this.name] }
  protected [symbols.extend](props?: any) { … }
  [symbols.resolveConfig](base?: T, head?: T): T { … }
  static [Symbol.hasInstance](instance: any) { … walks constructor prototype chains … }
}
```

* `symbols.invoke` lets a *service be a function*: `createCallable` builds a real `function` whose prototype chain is `service prototype → Function.prototype`, and `createTraceable`'s `apply` trap dispatches to `value[symbols.invoke].apply(proxy, args)`. This is how a service can be callable while still being a context-tracked object.
* `Service.init` is the async lifecycle hook. In `Fiber._runner.execute`, for a **constructor** plugin: `new runtime.callback(ctx, config)`, then run every `instance[symbols.initHooks]`, then `return instance?.[symbols.init]?.()`. A generator `[Service.init]()` yields disposers (as in `Group`) which become tracked effects of the fiber. Verified [T `plugin.spec.ts` "Service.init"].
* `Service.check` is the veto hook (see §4.3).
* `Service.config` is the *symbol* under which the service's config type is recorded so that `InjectKey` can type-restrict injectable names; `declare [Service.config]: T` on `Loader` etc.
* `Service.extend(props)` derives a variant instance; `Service.tracker` is why `ctx.foo.bar()` re-binds `this` correctly.

### 4.7 Consuming another plugin's service — the whole sequence

1. Provider: `ctx.provide('foo', impl)` or a `Service` subclass (auto-provide) → an effect; `store[realmSym('foo')] = impl`; `notify(['foo'])`.
2. Consumer: declares `inject: ['foo']` (or `@Inject('foo')`, or `ctx.inject(['foo'], cb)`) → the fiber's `_refresh` computes `epoch` from `impl.fiber.uid`; `INACTIVE` until the provider is ACTIVE.
3. Activation: `_reload()` snapshots `store = {..._store}` (commit ω) and runs the plugin callback with `ctx` whose `fiber.store` resolves `ctx.foo` via the Proxy.
4. Provider unload: `notify` → consumer's `_checkImpl` fails → `epoch = INACTIVE` → `_unload()`; but the consumer's committed `store` still holds the impl, so `ctx.foo` still resolves **during the consumer's own teardown**; `store = undefined` only at the end of `_unload`.
5. Re-provide by a different fiber: new `uid` ⇒ different epoch ⇒ consumer unloads then reloads.

---

## 5. Component and plugin lifecycle

### 5.1 The paper's calculus [P §4.1–4.2]

* **Component** (Definition 48): `ℭ_Γ ≔ (d : 𝔇_Γ) × (p : 𝔓_Γ) × ℑ_Γ^{d∪p}` — a triple `(d, p, e)`: the coeffect **specification** it reads, its coeffect **provision** (the keys it may install, and *no key outside p*), and its witnessed effect iterator.
* **Fiber** (Definition 49): `⟨d, p, e, π, σ, τ, θ⟩` where `π` is the parent fiber (or `root`), `σ` the fiber's own table (empty until it activates), `τ` the retirement flag, and
  ```
  Θ_Γ ≔ 𝖨𝗇𝖺𝖼𝗍𝗂𝗏𝖾 | 𝖱𝖾𝗅𝗈𝖺𝖽𝗂𝗇𝗀(i, g, ω) | 𝖠𝖼𝗍𝗂𝗏𝖾(g, ω) | 𝖴𝗇𝗅𝗈𝖺𝖽𝗂𝗇𝗀(g, ω)
  ```
  with `i` the remaining iterator, `g` the accumulator so far, and **ω the committed view** — `d_n → 𝔑`, the resolution the fiber activated against. `installed_n(γ) ≔ θ_n ≠ 𝖨𝗇𝖺𝖼𝗍𝗂𝗏𝖾`; an installed fiber resolves `k` to `m` when `ω_n(k) = m`.
* **Registry** (Definition 50): `F_γ : 𝔑 ⇀ 𝔉_Γ`, a finite partial map whose parent pointers form a tree rooted at `root`. The coeffect context is **derived, not stored** (eq. 46): `σ_γ ≔ ⋃{σ_m | m ∈ dom(F_γ), θ_m = 𝖠𝖼𝗍𝗂𝗏𝖾(−,−)}`. The union is well-defined because provisions are disjoint, so each key has exactly one possible provider (`provider_k(γ)`), *"fixed by the provisions and not by the state"*. **Consequence:** a `Reloading` or `Unloading` fiber reads its coeffects through its ω and provides none of its own — "a key its transition has already written is not yet one a dependent may activate against."
* **Target view** (Definition 53): `target_n(γ) = ⊥ if τ_n ∨ ¬(γ ⊧ d_n)`, else `(k ∈ d_n) ↦ provider_k(γ)`.
* **Quiescence** (eq. 49): every fiber settled at its target.
* **Relying** (Definition 54): `relied_n(γ) ≔ ∃m,k. m ≠ n ∧ installed_m(γ) ∧ ω_m(k) = n`.

**The nine rules** (§4.2). Orchestration rules (`γ ⇒ δ`, legal-when-premises-hold):
* **O-Insert**: `n ∉ dom F` ∧ `π ∈ dom F ∪ {root}` ∧ `(d,p,e) ∈ ℭΓ` ∧ `∀m. p ∩ p_m = ⌀` ⟹ add `⟨d,p,e,π,⌀,⊥,𝖨𝗇𝖺𝖼𝗍𝗂𝗏𝖾⟩`. The last premise is the single-source discipline.
* **O-Retire**: `τ_n = ⊤` (unconditional otherwise) ⟹ set `τ_n ≔ ⊤`. *"Retiring is a request, and the lifecycle rules are what carry it out."*
* **O-Remove**: `θ_n = 𝖨𝗇𝖺𝖼𝗍𝗂𝗏𝖾` ∧ `σ_n = ⌀` ∧ `∀m. π_m ≠ n` ⟹ delete the entry.

Lifecycle rules (`γ ⟶ δ`, taken unprompted):
* **L-Begin** (Inactive → Reloading): `θ_n = 𝖨𝗇𝖺𝖼𝗍𝗂𝗏𝖾` ∧ `ω = target_n(γ) ≠ ⊥` ⟹ `θ_n ↦ 𝖱𝖾𝗅𝗈𝖺𝖽𝗂𝗇𝗀(e_n, id_Γ, ω)`.
* **L-Iter**: `θ_n = 𝖱𝖾𝗅𝗈𝖺𝖽𝗂𝗇𝗀(i,g,ω)` ∧ `target_n(γ) = ω` ∧ `i(γ) = (δ, h, 𝖩𝗎𝗌𝗍(i'))` ⟹ `δ[θ_n ↦ 𝖱𝖾𝗅𝗈𝖺𝖽𝗂𝗇𝗀(i', g∘h, ω)]`.
* **L-Finish**: same, with `i(γ) = (δ, h, 𝖭𝗈𝗍𝗁𝗂𝗇𝗀)` ⟹ `δ[θ_n ↦ 𝖠𝖼𝗍𝗂𝗏𝖾(g∘h, ω)]`.
* **L-Divert** (Reloading → Unloading): `θ_n = 𝖱𝖾𝗅𝗈𝖺𝖽𝗂𝗇𝗀(i,g,ω)` ∧ `target_n(γ) ≠ ω` ∧ (`(δ,h) = (γ, id_Γ)` — abort the in-flight iteration — ∨ `i(γ) = (δ,h,−)` — let it land) ⟹ `δ[θ_n ↦ 𝖴𝗇𝗅𝗈𝖺𝖽𝗂𝗇𝗀(g∘h, ω)]`.
* **L-Leave** (Active → Unloading): `θ_n = 𝖠𝖼𝗍𝗂𝗏𝖾(g,ω)` ∧ `target_n(γ) ≠ ω` ⟹ `γ[θ_n ↦ 𝖴𝗇𝗅𝗈𝖺𝖽𝗂𝗇𝗀(g,ω)]`.
* **L-Unload**: `θ_n = 𝖴𝗇𝗅𝗈𝖺𝖽𝗂𝗇𝗀(g,ω)` ∧ `¬relied_n(γ)` ∧ `g(γ) = δ` ⟹ `δ[θ_n ↦ 𝖨𝗇𝖺𝖼𝗍𝗂𝗏𝖾]`.

Key structural facts [P]:
* **Accumulator discipline**: each iteration composes the yielded inverse as `g ∘ h`, "so that the accumulator applies the inverses in last-in-first-out order".
* **Why deactivation is two steps**: *"A component being torn down because its provider is going away is running its own teardown code, which may need the very coeffect that is being withdrawn... closing a connection pool typically means handing the connections back to whatever provided them."* Hence L-Leave separates the decision (stop providing, keep ω) from the act (L-Unload, discard ω last).
* **The guard does not deadlock**, because `σ_γ` unions only over ACTIVE fibers: once L-Leave marks `n`, its table leaves `σ_γ`, so no target view can name it and every consumer is already on its way out (Theorem 73).
* **The guard orders along coeffects, not the fiber tree**: a parent may revert while its child is still Unloading.
* **Instantiation** (Definition 52): an iteration of `e_n` may perform an O-Insert with `π = n`; the inverse it yields is the O-Retire of the created fiber. *"The inverse retires rather than removes, and the reason is that an inverse has to apply wherever it is reached"* — O-Remove carries premises an inverse cannot guarantee.
* **Nondeterminism**: the rules commit to no order among fibers; "no rule mentions a scheduler", so theorems hold for every scheduling policy.

### 5.2 Implementation [C][T]

`RegistryService` + `Fiber` implement the above with these exact correspondences (reconstructed from the algorithms and prose — the PDF's Table 2 columns are garbled by text extraction, so I did not rely on it):

| Paper | Code |
|---|---|
| `ctx.use` / O-Insert / O-Retire (Def. 52) | `ctx.plugin()`; the tracked-effect callback in the `Fiber` constructor; its returned closure |
| O-Remove | fiber removed from `runtime.fibers`, `uid` cleared in the disposer, `registry.delete` when the list empties |
| L-Begin, L-Iter, L-Finish | `_reload()` + `_execute(this._runner)` iteration loop |
| L-Divert | the epoch guard failing at an iteration boundary of `_execute`, **or** `_reload` chaining into `_unload` |
| L-Leave | `_updateState` returning `UNLOADING` before the transition task is created |
| L-Unload | `_unload()` and its inertial chaining |
| guard on L-Unload | `_unload()` awaiting the notified dependents (via `provide`'s disposer / `notify`) |
| `θ` | `fiber.state` (`LOADING` ≙ Reloading; `FAILED` carries the §4.4 error outcome) |
| `g` the accumulator | `fiber._disposables` |
| `ω` the committed view | `fiber.store` |
| `provider_k(γ)` | an `Impl` whose `fiber.state === ACTIVE` |
| `target_n(γ)`, ⊥ = Inactive | `fiber._runner.epoch`, recomputed by `_refresh()`; `⊥` = `'__INACTIVE__'` |
| inertia (§4.4) | `fiber.inertia` |

**Construction, in order** (`RegistryService.plugin` then the `Fiber` constructor):

1. `callback = this.resolve(plugin)` — throws `'invalid plugin, expect function or object with an "apply" method, received <typeof>'` for anything else; `this.ctx.fiber.assertActive()`.
2. `runtime = this._internal.get(callback) ?? { name: plugin.name, callback, fibers: new DisposableList(), Config: plugin.Config }` — note `name === 'apply'` is normalized to `undefined`.
3. `fiber = new Fiber(this.ctx, config, Inject.resolve(plugin.inject), runtime, getOuterStack)`; the return value is `Object.create(fiber)` with a `then` → `fiber.await()`.
4. `Fiber` constructor (with a runtime):
   * `uid = parent.registry.counter` (pre-increment, so root is 0 and the first plugin is 1);
   * `this.ctx = this.context = parent.extend({ fiber: this })`;
   * non-null inject configs are written into a fresh `ctx[Context.intercept]`;
   * build `_runner` whose `execute` handles both function and constructor plugins (see §4.6) and whose `epoch` starts as `INACTIVE`;
   * `this.context.emit('internal/plugin', this)` — **fires at construction, before any inject check**;
   * `for (const name of Object.keys(this.inject)) this._checkImpl(name)`;
   * then, and this is the crucial part:
     ```ts
     this.dispose = parent.fiber.effect(() => {
       const remove = runtime.fibers.push(this)
       try { this.config = resolveConfig(runtime, config); this._refresh() }
       catch (error) { this.ctx.logger.error(error); this._error = error }
       return async () => {
         this.uid = null
         this.context.emit('internal/plugin', this)     // second emit: unload
         if (this.ctx.registry.has(runtime.callback)) {
           remove()
           if (!runtime.fibers.length) this.ctx.registry.delete(runtime.callback)
         }
         this._setEpoch(INACTIVE)
         while (this.inertia) await this.inertia           // drain the transition
       }
     }, 'ctx.plugin()')
     ```
     **Instantiating a component is itself a tracked effect on the parent fiber.** That single fact gives: cascade-unload when the parent unloads, `INACTIVE_EFFECT` for any effect created after the parent is gone, and nested-plugin trees.
5. The **root fiber** is degenerate: `uid = 0`, `ctx = parent`, `state = ACTIVE`, `store = {}`, `epoch = ''`, `dispose = () => this.restart()`.

**Transition methods.**

```ts
private async _reload() {
  this.store = { ...this._store }                       // commit ω  (L-Begin's ω = target)
  const oldEpoch = this._runner.epoch
  try { await Promise.resolve(); await this._execute(this._runner) }   // run the component's iterator
  catch (reason) { this.ctx.logger.error(reason); this._error = reason; this._runner.epoch = INACTIVE }
  this._updateState(() => {
    if (this._runner.epoch === oldEpoch) this.inertia = undefined          // settled ACTIVE
    else { this.inertia = this._unload(); return FiberState.UNLOADING }    // L-Divert → chain
  })
}

private async _unload() {
  await Promise.all(this._disposables.clear().map(/* composeError + log */))
  this.store = undefined                                                    // discard ω LAST
  this._updateState(() => {
    if (this._runner.epoch === INACTIVE) this.inertia = undefined           // settled INACTIVE
    else { this.inertia = this._reload(); return FiberState.LOADING }        // chain
  })
}

async await()  { while (this.inertia) await this.inertia; if (this._error) throw this._error; return this }
async restart(){ this.ctx.fiber.assertActive(); this.ctx.fiber._setEpoch(INACTIVE); this.ctx.fiber._refresh(); await this.ctx.fiber.await() }

update(config, noSave = false): Awaitable<void> {
  const fiber = this.ctx.fiber; fiber.assertActive()
  config = resolveConfig(fiber.runtime!, config)
  const result = fiber.context.waterfall(fiber, 'internal/update', config, noSave, () => {
    fiber.config = config; fiber._error = undefined; return fiber.restart()
  })
  if (result === undefined) return                 // a listener vetoed the restart
  const task = Promise.resolve(result); task.catch(() => {}); return task
}
```

**This mutual chaining is the inertial state machine of §4.4 [P]:** *"once entered, a transition runs to completion before the system responds to a target-state change."* Two levels of target check give the paper's two mechanisms: the **transition level** (`_reload`/`_unload` re-check epoch at completion → inter-transition chaining) and the **iteration level** (`_execute`'s async-iterator epoch check → intra-transition staleness, `L-Divert`).

**Verified lifecycle behaviours** [T `fiber.spec.ts`]:
* *inertia lock 1*: while `LOADING`, withdrawing and re-providing the dependency does not interrupt; at ~1200 ms the fiber reaches `UNLOADING`, then re-provides and reaches `LOADING` → `ACTIVE`. The transition runs to completion first.
* *inertia lock 2*: a re-provide that happens *during* the load makes the fiber land `ACTIVE` directly (target changed to a different provider but the epoch comparison at completion catches it).
* *inertia lock 3*: after the provider's `dispose()` completes, the dependent is `PENDING` (≙ Inactive).
* *plugin error*: a throwing plugin yields `FAILED`, its effects already installed are reverted, a sibling fiber with valid config is unaffected, and its listeners are removed (the callback count stays 0).
* *failed fiber does not re-enter on dependency refresh*: `FAILED` is sticky; only `update()` clears it.
* *update recovers a failed fiber*; *update surfaces a failed reload to its caller* (`await fiber.update({})` rejects with `'boom'` and leaves `FAILED`); *update does not leak a dropped failure*.
* *dispose error*: `fiber.dispose()` resolves even when a disposer throws.
* *restart wrapped fiber*: `restart()` re-runs the callback.
* *update config while injected service reloads*: a consumer whose provider reloads sees the *new* provider value on its own subsequent `update`, and after settling the wrapper's own `config`/`state`/`inertia` properties are absent (they live on the prototype chain of the `Object.create(fiber)` wrapper).

**Plugin deletion.** `RegistryService.delete(plugin)` disposes every fiber of that runtime and removes the runtime; `has`/`get`/`keys`/`values`/`entries`/`forEach` expose `dom F_γ`. Subtle: the fiber disposer only calls `remove()` if `ctx.registry.has(runtime.callback)` — so `delete()` deliberately leaves the fibers in `runtime.fibers`, which the HMR plugin relies on to rebuild from.

**Naming.** `Fiber.name` walks up `runtime?.name` through the parent chain and falls back to `'root'`; `Symbol.for('nodejs.util.inspect.custom')` on `Context` renders `Context <name>`.

---

## 6. The declarative component loader

### 6.1 Paper [P §5.2.1]

**Definition 81** — an **entry** records: `id` (stable reconciliation key), `url` (module to instantiate), `isolate`, `intercept`, `config`, `disabled`. *"An entry can serve as a faithful specification because what supports a fiber is exactly what an entry records"* — the support set reads `τ, π, d, p` and nothing else.

**Reconciliation dispatch**, per changed field:
| changed field | operation |
|---|---|
| `id`, `url` | rebuild the entry (identity or component changed) |
| `isolate` | reassign the entry's realms (Algorithm 7) |
| `intercept` | update in place — metadata is consulted at read time, so no reload |
| `config` | hand it to the component, which decides (typically diff and reload only on a material change). An `@cordisjs/group` entry's config *is* its child list, so it applies a **keyed diff over child ids**, recursing |
| `disabled` | unload when set, reload when cleared |

**Why incremental reconciliation is sound** [P]: Theorem 80 (the quiescent state is a function of the final configuration alone — whatever instantiations and retirements happen on the way), Theorem 73 (the system does quiesce), Corollary 69 (a departing fiber contributes nothing to the state), Theorem 70 (entries may be instantiated together with **no load order to arrange** — *"the loader loads modules concurrently, where bringing up a large configuration spends its time"*).

**Managed realms** (Algorithm 7): `isolate: true` selects a **local** realm private to the entry and tagged by its id, carried along when the entry moves; a string selects a **global** realm shared by every entry naming that string. A realm is discarded once no entry names it. Reassignment turns on which keys changed realm, whether the entry is itself the provider at a changed key, and which dependents to notify; the middle question is answered with **delimiters** `δ_k`, one symbol per key under which each context stores its own tag; since the tag is written on a context and inherited by its descendants, `γ'[δ_k] = d₁ ⟺ γ' is derived from the entry's context`, so the loader can tell whether the binding at `k` is the entry's own and must move with it.

### 6.2 Implementation — the exact config schema [C]

**`EntryOptions`** (`loader/src/config/entry.ts` + augmentation in `config/isolate.ts`) — the authoritative schema, and note it **differs from the paper's field list**:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `id` | `string` | auto: 8 random hex chars | stable reconciliation key; the tree-qualified id is ancestor ids joined by `':'` |
| `name` | `string` | — | module specifier for `import()` (or `cordis:<key>` for `loader.builtins`). **This is the paper's `url`.** |
| `config?` | `any` | — | the plugin config; **for a group entry, the child `EntryOptions[]`** |
| `group?` | `boolean \| null` | — | marks a group wrapper (groups are never `disabled`) |
| `disabled?` | `boolean \| null` | — | disables this entry and all descendants |
| `inject?` | `Inject \| null` | — | merged into the fiber's inject via `Inject.resolve` |
| `intercept?` | `Dict \| null` | — | replaces `entry.ctx[Context.intercept]` |
| `isolate?` | `Dict<true \| string> \| null` | — | `true` = local realm, string = named global realm |

Key ordering is normalized by `sortKeys(options, prepend = ['id','name'], append = ['config'])` with the rest sorted by `localeCompare`, making file round-trips stable.

The **loader itself has no config-file parser and no schema for the entry tree.** `Loader.Config` is exactly `{ baseUrl?: string }`. YAML/JSON parsing lives in `@cordisjs/plugin-include` (js-yaml + a custom `!!js` tag producing `{__jsExpr}` nodes). The `@cordisjs/plugin-group` package is literally `import { Group } from '@cordisjs/plugin-loader'; export default Group`.

### 6.3 Implementation — reconciliation [C]

`Entry.update(options, create = false, force = false)` is the whole per-entry algorithm:

```ts
const legacy = { ...this.options }
if (create) this.options = options as EntryOptions
else for (const [key, value] of Object.entries(options))
  if (isNullable(value)) delete this.options[key]; else this.options[key] = value
sortKeys(this.options)

if (this.disabled) { this.fiber?.dispose(); return }                  // disabled: unload, no commit
if (this.fiber?.uid) {
  const diff = Object.keys({ ...this.options, ...legacy })
    .filter(key => !deepEqual(this.options[key], legacy[key]))
  if (!diff.length && !force) return                                  // unchanged: left completely alone
  this.context.emit('loader/partial-dispose', this, legacy, true)
  await this._patchContext(diff)
} else await this.init()                                              // no live fiber: full init
```

`_patchContext(diff)`:

```ts
await this.context.waterfall('loader/patch-context', this, async () => {
  Object.setPrototypeOf(this.ctx, this.parent.ctx)        // re-parent the derived context (moves!)
  if (this.fiber?.uid && (diff.includes('config') || this.options.group))
    await this.fiber.update(this._resolveConfig(this.fiber.runtime!.callback), /*noSave*/ true)
})
```

So the *least disruptive operation per field* is realized as: patch the entry's derived context always; run the fiber's own update **only** when `config` changed or the entry is a group. `id`/`name`/`inject`/`isolate`-only changes on a live entry do **not** re-run the plugin and do **not** re-import the module. `noSave = true` suppresses the loader's `internal/update` write-back hook, so loader-driven changes never round-trip to disk.

**`EntryGroup.update(config)` — the group keyed diff:**

```ts
const oldConfig = this.data; this.data = config
const oldMap = Object.fromEntries(oldConfig.map(o => [o.id, o]))
const newMap = Object.fromEntries(config.map(o => [o.id ?? Symbol('anonymous'), o]))
const ids = Reflect.ownKeys({ ...oldMap, ...newMap }) as string[]
await Promise.all(ids.map(async (id) => {
  if (newMap[id]) await this.create(newMap[id]).catch(e => this.ctx.logger.error(e))
  else this.remove(id)
}))
```

`create(options)` reuses `tree.store[id] ??= new Entry(loader)`, re-parents it (`entry.parent = this` — entries move between groups), and calls `entry.update(options, /*create*/ true, /*force*/ true)`. `force` is why unchanged entries enter the diff path; the deep-equality check then returns early. `remove(id, isDispose = false)` **deletes `tree.store[id]` before `entry.fiber?.dispose()`** — deliberately, because the loader's `internal/plugin` handler distinguishes "removed by the loader" from "disposed itself" by checking the store. It then unlinks from `data` and emits `'loader/partial-dispose'(entry, options, false)`.

`Entry.init()`/`_init()`: `import` the module (memoized in `_initTask`; errors logged, not thrown) → `unwrapExports` (`.default ?? exports`, then esModule interop) → `_patchContext([])` → `registry.plugin(plugin, _resolveConfig(plugin), getOuterStack)`. **Reloading an existing entry never re-imports the module.**

**Config interpolation**: `_resolveConfig(plugin)` returns `options.config` verbatim for a group plugin, otherwise `interpolate(ctx, config)`, which walks arrays/objects and evaluates `{__jsExpr: "…"}` nodes through `new Function('ctx','expr','with (ctx) { return eval(expr) }')`. This is the only expression mechanism in the loader; `@cordisjs/plugin-include` supplies the `!!js` YAML tag that produces these nodes and defers nested expressions until the target entry activates.

**Loader service surface** (verified):

```ts
class Loader extends EntryTree {
  envData                                    // process.env.CORDIS_SHARED parsed, else { startTime: Date.now() }
  internal = ModuleLoader.fromInternal()
  builtins: Dict<any>
  get root(): EntryGroup;  get store(): Dict<Entry>
  entries(): Generator<Entry>
  getTasks(): Promise<void>[];  await(): Promise<void>
  create(options, parent = null, position = Infinity): Promise<string>
  remove(id: string): void
  update(id: string, options, parent?, position?): Promise<void>
  resolve(id): Entry            // throws `cannot resolve entry <id>`
  resolveGroup(id | null): EntryGroup   // throws `entry <id> is not a group`
  import(name, getOuterStack?): Promise<any>
  unwrapExports(exports); locate(fiber?); showLog(entry, type); exit()
  commit(): void                // explicit no-op: the root tree lives in memory only
  [Service.check]()             // false while `config.await && getTasks().length`
}
```

`ctx.loader` is the service key. `Loader.Intercept` = `{ await?: boolean }`, so `ctx.inject({ loader: { await: true } }, …)` makes a consumer wait for the entry tree to settle — a clean example of a provider vetoing availability through `Service.check`. Loader events: `'exit'(signal)`, `'loader/config-update'()`, `'loader/entry-init'(entry)`, `'loader/partial-dispose'(entry, legacy, active)`, `'loader/patch-context'(entry, next)`. `Fiber.entry?: Entry` is the back-pointer.

**Write-back and self-dispose.** Two global `internal/update` hooks: one (prepended) writes a plugin's self-updated config back into the entry tree using `runtime.Config['simplify']` to unparse, then commits; one logs `'reload'`. The `internal/plugin` handler implements the *bidirectional* binding Definition 81 mentions: a component that disables itself has the change written to its entry — the handler has 7 documented guard cases (fiber created / not tracked / child plugin / deletion-on-behalf-of-plugin-HMR / tree disposing / entry already removed by the loader / disposed by loader behaviour) and only a genuine self-dispose sets `entry.options.disabled = true` and commits.

### 6.4 `@cordisjs/plugin-include` — config-file durability

`class Include extends EntryTree` with `static inject = ['loader']` and `Config { path: string; initial?: any[]; patches?: PatchOptions[]; enableLogs?: boolean }`. It is the parser/persister the loader deliberately is not. Mechanism [C]:

* **Parse**: `yaml.load(content, { schema })` with `schema = yaml.JSON_SCHEMA.extend(JsExpr)` where `JsExpr` is a `yaml.Type('tag:yaml.org,2002:js', …)` constructing `{__jsExpr}`; JSON via `JSON.parse`. Extension whitelist `.json/.yaml/.yml`; anything else throws `extension "<ext>" not supported`; a non-array document throws `ConfigFileError('validate', …, TypeError('config file must be a top-level array'))`; `ConfigFileError { name='ConfigFileError', stage: 'read'|'parse'|'validate' }`.
* **Journal**: `Journal = Map<string, JournalRecord>` with `Remove {kind:'remove'}` and `Upsert {kind:'upsert', created, parent, position, changes}`. `Loader`-style mutations reach it through `EntryTree.commit(change: EntryChange)` → `record(journal, change, parent)` → `dirtyWrite = true`. `mergeRecords` cancels create+remove and unions upserts. `applyJournal(data, journal, filter, warn)` overlays the journal on the file-derived tree, placing/moving entries (`cannot place entry %C: group %C not found`).
* **Three-way reconcile**: `reconcile(journal, base, theirs, fileOwned)` returns `Conflict[]`; **the file always wins** — conflicts are logged as `config conflict in %C: entry %C %s; file wins`.
* **Ownership**: `PatchIndex` classifies each key as file-owned, patch-owned, or insert-owned (`EntryOwner.File|Patch|Insert`); `routeJournal` sends file-owned changes to the data and patch-owned ones into the `patches` array. `PatchOptions = { id?, insert?: EntryOptions[], name?, config?, group?, disabled?, inject?, intercept?, isolate?, … }`; `applyPatches` clones, applies inserts (which must name an existing group) and overrides, warning `patch: name mismatch for %C …`.
* **Atomic write**: temp file `.<basename>.<pid>.<seq>.tmp` + `rename`, with `StaleWriteError` if the file changed under it, transient-error retries (`RENAME_RETRIES = 10`, `RENAME_BACKOFF = 20` ms, codes `EACCES|EPERM|EBUSY`), and on failure the journal is *kept in memory* with a warning (`config file %C is read-only, %d pending change(s) kept in memory`).
* **Coalescing**: `_scheduleApply()` computes `applyJournal(applyPatches(cache.data, patches))` and stores it in `_pendingTree`; a single `_applyTask` loop drains it, so **only the latest intended tree is ever applied**.
* **Id stability**: `_assignIds` matches anonymous entries against the previous map by `(parent, deepEqual(options))` so editing a neighbour does not restart an entry.

### 6.5 Hot module replacement — `@cordisjs/plugin-hmr`

Paper [P §5.2.2]: *"HMR applies the revertible-effect pattern at the module level... Because a fiber already bounds all of its component's effects and coeffects, a module that is itself a component can be replaced through fiber operations alone: disposing the old fiber recovers everything the component installed, and a new fiber instantiated from the reloaded module reinstalls it. HMR therefore needs no developer-annotated acceptance boundaries, as opposed to Webpack or Vite HMR."* Three phases: classification (Alg. 8), stale-entry detection (Alg. 9), transactional reload (Alg. 10).

Implementation [C] — `class Hmr extends Service`, `super(ctx, 'hmr')`, `@Inject('loader') @Inject('timer')`:

* **Config** (schemastery, `z.object(...)`, the only schema in the loader tier): `base?: string`, `root: string[] = ['.']`, `ignored: string[] = ['**/node_modules','**/.*','cache','data']`, `debounce: 100` ms (`z.natural().role('ms')`), extending `ChokidarOptions`. Events: `'hmr/change'(url)` and `'hmr/reload'(stalePlugins: Map<Plugin, StalePlugin>)`.
* **Watching**: chokidar v4; `ctx.hmr.watch(path, callback)` is itself a tracked effect (label `'ctx.hmr.watch()'`) registering into `watchers: Map<file, Set<callback>>`; the chokidar watch set only ever grows, and disposing removes only the callback. `partialReload` is `ctx.debounce(…, config.debounce)`.
* **Externals / full restart**: `externals` is the transitive dependency closure of the CLI entry (`process.argv[1]`); a change in an external calls `loader.exit()` (full restart) rather than HMR.
* **Accept/decline** (Alg. 8 ↔ `analyzeChanges`): `accepted` seeded from `stashed` (changed files present in the Node internal `loadCache`), `declined` from `externals`; a module is accepted once any of its imports is accepted, declined once all are declined; `node:` and `/node_modules/` are excluded; anything left undecided (an import cycle) is forced to declined. `getLinked(url)` reads the internal ModuleJob's `linked` array.
* **Stale detection** (Alg. 9): for each candidate plugin's module, `loadDependencies(job, declined)` collects transitive imports respecting `declined` as a boundary; the entry is stale iff that tree intersects `accepted`; stale modules are folded into `accepted` for invalidation.
* **Stage 1 — re-import, all-or-nothing**: invalidate *both* the internal ESM `loadCache` and `require.cache` (backing up both), then re-`import` each stale module; a non-plugin export throws `invalid plugin at <relpath>, expect function or object with an "apply" method, received <typeof>`; **any failure here runs `rollback()`, restoring both caches** — no plugin has been touched yet. `error.ts` renders esbuild build failures with `@babel/code-frame` and otherwise warns.
* **Stage 2 — unload**: snapshot `[...runtime.fibers]`, then `ctx.registry.delete(plugin)`; **plus** filter out fibers with an inactive ancestor (`skip plugin at %C (inactive ancestor)`), because "everything below a fiber that is going away is rebuilt by that fiber".
* **Stage 3 — reload**: for each stale fiber, `while (fiber.inertia) await fiber.inertia`, then `fiber.parent.registry.plugin(replacement, fiber.config, getOuterStack)`, re-pointing `newFiber.entry`/`entry.fiber`.
* **Preserved across reload**: the fiber's `config` object verbatim, the entry identity, the parent registry and context (hence isolate/intercept context and loader options). **Not preserved**: the plugin function object, its registered effects (disposed in stage 2), and the services it provided.
* **⚠ No rollback in stage 3.** The code comment is explicit: *"No rollback: a plugin that fails to load is left failed, exactly as it would be on a cold start. It stays registered and keeps its parent and config, so the next change to the file retries it."* See §12.

---

## 7. Config schema validation

* **Kernel contract** [C]: `Plugin.Base.Config?: StandardSchemaV1<any, T>` — *any* Standard Schema v1 validator (schemastery, zod, valibot, arktype). `Plugin.Transform<S,T> = { schema?: true; Config: (config: S) => T }` supports transforming schemas.
* **Validation point** (`fiber.ts`):
  ```ts
  export function resolveConfig(runtime: Plugin.Runtime, config: any) {
    if (!runtime.Config) return config
    const result = runtime.Config['~standard'].validate(config)
    if ('then' in result) throw new TypeError('Async config validation is not supported')
    if (result.issues) throw new ValidationError(result.issues)
    else return result.value
  }
  ```
  **Synchronous only** — async validation is explicitly rejected. The validated/transformed value is assigned to `fiber.config` and passed as the second argument to `apply(ctx, config)` / `new Plugin(ctx, config)`; a `Transform` schema's output type is what the plugin sees.
* **Error surface** [C]:
  ```ts
  class ValidationError extends TypeError {
    name = 'ValidationError'
    constructor(issues) {
      super(`invalid config:\n` + issues.map(i => i.path ? `  - ${i.message} (at ${i.path.join('.')})` : `  - ${i.message}`).join('\n'))
    }
  }
  ```
  It also carries a `Symbol.for('ValidationError')` marker on the prototype.
* **Where a validation failure lands**: in the `Fiber` constructor, `resolveConfig` runs inside the tracked effect, so a throw is caught → `ctx.logger.error(error)` → `_error` set → the fiber is `FAILED` (and it will not re-enter on dependency refresh). In `Fiber.update()`, `resolveConfig` throws *before* the `internal/update` waterfall, so the caller's promise rejects.
* **Schema extras the loader relies on** [C]: the schema object may carry `merge(...)` (used by `Service[Service.resolveConfig]` to combine intercepted configs) and `simplify` (used by the loader to unparse a revised config back into the entry tree). Schemastery's `.role()`/`.i18n()`/`.default()` produce these; `@cordisjs/plugin-hmr` uses `z.natural().role('ms').default(100)` and `.i18n({'en-US','zh-CN'})`.
* **Two different validations, do not conflate them**: (a) *per-component* config, validated by the component's own `Config` schema at fiber construction/update; (b) *the config file*, validated structurally by `Include` (top-level array, extension whitelist, YAML/JSON parse) — there is no schema for the entry tree itself. Additionally, `interpolate` evaluates `{__jsExpr}` nodes *after* loading, so a config file's `config` values may be computed from the entry's own context (`ctx.serviceName`).

---

## 8. Events, and whether they are effects

### 8.1 Theory

Events do not appear in the paper's calculus as a primitive. The relevant facts are: (i) a plugin's interactions must be *context-mediated stages* (Definition 56) — an operation at a declared key, a provision, or an instantiation; (ii) an implementation may add mechanisms with **derived realization**, which "leaves the input intact and returns a fresh context deriving from it, with the identity as its inverse" (Definition 23). Cordis' event system is exactly such a mechanism: **registration is an effect; emission is not.**

### 8.2 Implementation [C]

```ts
type DispatchMode = 'emit' | 'parallel' | 'serial' | 'bail' | 'waterfall'
interface EventOptions { prepend?: boolean; global?: boolean }
interface Hook extends EventOptions { ctx: Context; callback: (...args:any[]) => any }

class EventsService {
  _hooks: Record<keyof any, Hook[]> = Object.create(null)
  on(name, listener, options?): () => boolean
  once(name, listener, options?): () => boolean
  emit(name, ...args): void
  parallel(name, ...args): Promise<void>
  serial(name, ...args): Promisify<ReturnType>
  bail(name, ...args): ReturnType
  waterfall(name, ...args): ReturnType
  dispatch(type, args)          // @deprecated
}
```

* **Registration is a tracked effect** — this is the direct answer to the question:
  ```ts
  private register(label, name, callback, options) {
    const method = options.prepend ? 'unshift' : 'push'
    return this.ctx.fiber.effect(() => {
      const hooks = this._hooks[name] ??= []
      hooks[method]({ ctx: this.ctx, callback, ...options })
      return () => this.unregister(name, callback)
    }, label)
  }
  on(name, listener, options?) {
    if (typeof options !== 'object') options = { prepend: options }
    this.ctx.fiber.assertActive()
    listener = this.ctx.reflect.bind(listener)          // traces args/this through the context
    const result = this.bail(this.ctx, 'internal/listener', name, listener, options)
    if (result) return result
    return this.register(`ctx.on(${JSON.stringify(name)})`, name, listener, options)
  }
  ```
  So a listener is installed as an effect of the registering fiber, carries an `EffectMeta` label `ctx.on("<name>")` (visible in `getEffects()`), and is removed automatically, in LIFO order with the fiber's other effects, when the fiber unloads. Removal (`unregister`) only splices the array, so teardown cannot fail. This is why nested-plugin teardown restores the exact prior hook set [T `plugin.spec.ts` "compare snapshot"].
* **Dispatch modes**, exactly [C]:
  * `emit` — synchronous, all callbacks in registration order, return values ignored.
  * `parallel` — `Promise.allSettled`, throws `AggregateError` if any rejected.
  * `serial` — awaits each in order, returns the first bailed value.
  * `bail` — synchronous, returns the first bailed value.
  * `waterfall` — middleware; **the last positional argument is popped as the innermost `next`**, each callback receives `(...args, next)`, `next()` called twice throws `next() called multiple times`, and not calling `next()` short-circuits.
  * Bail predicate: `isBailed(v) = v !== null && v !== false && v !== undefined`.
  * Argument resolution (`_resolve`): if `args[0]` is an object or function it is shifted as `thisArg`, then the name is shifted; a non-`internal/` name first emits `'internal/dispatch'(mode, name, args, thisArg)` (an observability hook); then hooks are filtered by `hook.global || !filter || filter.call(thisArg, hook.ctx)` where `filter = thisArg?.[Context.filter]` — **the realm gate for events**.
* **What the kernel itself uses** [C]: `EventsService`'s constructor installs two hooks. `'internal/listener'` (a `bail` dispatch) lets the fiber capture `internal/update` listeners into `fiber._hooks['internal/update']` unless `options.global`. A global, prepended `'internal/update'` handler chains those per-fiber hooks in front of `next`, which is how `Fiber.update()` offers a veto/middleware seam to plugins and to the loader's config write-back.
* **Declared `Events` interface** (the declaration-merging target for plugins) [C]: `internal/plugin(fiber)`, `internal/status(fiber, oldValue)`, `internal/service(this: Context, name, value)`, `internal/update(this: Fiber, config, noSave, next)`, `internal/get(ctx, name, error, next)`, `internal/set(ctx, name, value, error, next)`, `internal/listener(this: Context, name, listener, prepend)`, `internal/dispatch(mode, name, args, thisArg)`, plus `[key: symbol]: (...args) => any`. Plugin packages add their own via `declare module 'cordis' { interface Events { … } }`.
* **The docs site's framing** [docs]: dispatch mode is part of an event's public contract, and new harness events tag their mode with `@mode` so generated catalogues can cross-check declaration against dispatch site; waterfall is for wrapping, bail for "stop at the first decision", emit/parallel/serial for observation/fan-out/ordered execution.

**Are emissions effects?** No. `emit`/`parallel`/`serial`/`bail`/`waterfall` return nothing tracked and have no inverse: emitting is `id_Γ` in the paper's sense. A listener that performs side effects must itself call `ctx.effect`, `ctx.on`, `ctx.provide`, etc. The paper's §6.1 boundary discussion is the right frame: an event *notification* crosses the boundary (it is an emission), while the listener's *acquisition* of resources stays inside it as a tracked effect of the listener's own fiber.

---

## 9. The observational-equivalence discipline that constrains interleaving

### 9.1 Why `=` is too strong [P §3.3.2]

*"The recovery guarantee of Section 3.1 asserts an equality of states (Theorem 7), which is an idealization, because the physical state cannot be recovered as it stood. For example, `free` releases a block to the allocator without restoring the layout the heap had before `malloc`; and a generative name is not restored by the inverse that discards it, since the next creation draws a fresh one."* The equalities are therefore read up to an **observational equivalence ≃**, and the relation depends on what the observer is given: *the coeffects a context carries*, and *the operations of a value's key*.

### 9.2 The relation [P]

* **Tests** (Definition 31): a test over a key's operation set `A` is a finite word whose letters are forward maps and yielded inverses of the effect functions `a(x)`, each applied to the value the previous letters left; its outcomes are those of the forward letters. `v ≈_A v'` iff every test is defined at both or neither and yields equal outcomes. **`≃_k ≔ ≈_{A_k}`** (eq. 33). Lemma 32: each `≃_k` is an equivalence that every operation of `A_k` respects, and it is the **coarsest** such relation — so it doubles as a proof principle: to relate two values, exhibit any equivalence the operations respect that contains the pair.
* **Contexts** (Definition 33): `σ ≃_S σ' ≔ dom(σ)∩S = dom(σ')∩S ∧ ∀k ∈ dom(σ)∩S. σ(k) ≃_k σ'(k)`; `γ ≃_S γ'` by the coeffect projection. `≃ ≔ ≃_K` is the finest; `≃_S` forgets the keys outside `S` as well. **"The part of a state that no key binds is thereby forgotten"** — which is exactly what lets heap layout and generative names fall outside the relation.
* **Lifting** (Definition 34): along `→` by `f ≃_S g ≔ ∀γ,γ'. γ ≃_S γ' → f(γ) ≃_S g(γ')`; componentwise on products; casewise on `Maybe`; **coinductively (greatest relation) on recursive types**. A map *respects* `≃_S` when `f ≃_S f`. Lemma 35: `≃_S` is a **partial** equivalence on maps/iterators (symmetric and transitive), so related members each respect it. Respect is a condition, not a given: `f ≃_S f` demands related outputs at *every pair of related inputs*, not just equal ones, so a map that branches on a key outside `S` respects `≃` and fails to respect `≃_S`.
* **Witnessed up to ≃_S** (Definition 36): `𝔈^S_Γ ≔ (e : Γ → Γ × (Γ→Γ)) × (e ≃_S e) × (∀γ δ g. (δ,g) = e(γ) → g(δ) ≃_S γ)`. Setting `≃` to equality recovers Definition 8. **"The key set is where a component's declarations enter: what Section 4 holds an effect function to is 𝔈^S_Γ at the keys that component names."** Definition 37 is the same for iterators, and Lemma 38: **every equality of §3.1 holds with `=` replaced by `≃`**, and every reachable accumulator respects `≃`. Lemma 39: a context-mediated iterator whose stages occur in `S` lies in `ℑ^S_Σ` — so the discipline of Definition 30 *implies* the parametricity that makes the claims modular.

### 9.3 Independence — the actual constraint on interleaving [P §3.4]

Two situations demand more than "revert at the state you applied at": an inverse run while *later* effects are still in place (removing one component from a running system), and one sequence interleaving several components' effects. The question is **commutation**.

* **Transformation monoid** (Definition 40): `reach(i)` is the least continuation-closed set of iterators containing `i`; `𝔐(i)` is the submonoid of `Γ→Γ` generated by **the forward maps and the yielded inverses of every iterator in `reach(i)`** — "the operations of a key … over every argument". Lemma 41: commutation is settled on generators, and `⋄` enlarges no transformation monoid.
* **Independence of iterators** (Definition 42): `i, j` are independent when
  1. `∀f ∈ 𝔐(i), g ∈ 𝔐(j). f ∘ g = g ∘ f` — every transformation of one commutes with every transformation of the other, **forward maps paired with foreign inverses included**; and
  2. `∀i' ∈ reach(i), g ∈ 𝔐(j), γ. pr₂,₃(i'(g(γ))) = pr₂,₃(i'(γ))` (and symmetrically) — **neither one's transformations disturb what the other yields, inverse *and continuation* alike.**
  A family is *pairwise independent* when this holds for every distinct pair; a family may repeat an iterator, and holding one independent of itself is holding `𝔐(i)` commutative. Note commutation under `⋄` is *different*: `e₁ ⋄ e₂ = e₂ ⋄ e₁` compares composite forward maps and composite inverses, whereas independence relates each transformation of one to each transformation of the other.
* **Theorem 43 (the payoff)**: pairwise independent effects applied in order from γ₀, then reverted **in the order of any permutation**, reach γ₀. *"Under independence an inverse may be run at a state later effects have moved, and it withdraws there its own contribution and nothing else, whatever order the inverses are applied in."* This is what makes incremental reconciliation (removing one component from a running system) sound.
* **Coeffect commutativity** (Definitions 44–46): operations `a, a'` are independent when their lifts are independent as effect functions at every pair of arguments **and** neither disturbs the other's *outcome* (`pr₃(a_Σ(x)(g(σ))) = pr₃(a_Σ(x)(σ))`). Key `k` is **commutative** when any two of its operations are independent, each also from itself. Theorem 45: **operations at distinct keys are independent outright.** Definition 46: a *witnessed* coeffect carries a proof of commutativity as its third constituent, "supplied where the definition is written rather than checked where it is used" — so **the obligation falls on the provider of the key and on no consumer.**
* **Theorem 47**: two context-mediated iterators are independent if their provisions/declarations are mutually disjoint (`P₁∩S₂ = P₂∩S₁ = ⌀`) and every key at which both operate is commutative. **Only that disjointness remains to be checked of a pair** — and Section 4 reads it straight off the two components' declared provisions.
* **How to discharge commutativity in practice** [P §3.4.2] — this is the design procedure to port:
  * A key whose value is a **table of registered entries**, where each registration takes an entry of its own, is commutative: *"the operation draws an identifier for the entry it adds and the inverse it yields removes that entry, so two registrations name two entries whatever they register."* Route tables and event-listener sets are the representative cases. **This is exactly what `EventsService.register`/`unregister` do and what `DisposableList`'s serial numbers are for.**
  * A key whose value is an **ordered chain** (middleware) is **not commutative**: *"a middleware inserted before another sees a different request, and neither order can be withdrawn without disturbing the other."* This is why ordered event semantics must be expressed as a *declared coeffect boundary* (the provider imposes order internally) rather than as independently-reverting effects.
  * An **allocator** key is commutative iff no operation compares the handles *as an outcome*: `mmap`/`creat` (any unused address/inode) commute; POSIX `open` (lowest available fd) does not, precisely because the fd is an outcome compared by equality. More generally, **withholding an outcome the callers do not need coarsens `≃_k` and can move a key across the division** (this is the "scalable commutativity rule" reading).

### 9.4 The global theorems [P §4.3]

* **Theorem 68 (Recovery exactness)**: for an episode of fiber `n` open at `b`, at any `u ≥ b`, with `t₁ < … < t_l` the steps in `[b,u)` taken by *other* fibers: `g^u_n(γ_u) ≃_K (Ψ_{t_l} ∘ … ∘ Ψ_{t_1})(γ_b)`. I.e. **applying `n`'s accumulator leaves every fiber's table exactly where those same foreign steps would have left it had `n` never begun** (control fields excluded from the comparison).
* **Corollary 69 (Terminal recovery)**: when the episode closes, `γ_{u+1} ≃_K (Ψ_{t_l} ∘ … ∘ Ψ_{t_1})(γ_b)` and `σ_n = ⌀` — a departing fiber contributes nothing, which is what lets the loader rebuild one entry without disturbing its neighbours.
* **Theorem 70 (Ordering / spatial composability, global form)**: `L-Begin(m) ⇒ γ_t ⊧ d_m`; and for a dependent `m` resolving key `k` to provider `n`: `ω^t_m(k) = n` throughout `m`'s episode; `n`'s episode opens strictly before `m`'s and, if it closes, closes strictly after `m`'s; `k` stays in `dom(σ^t_n)`, moving only by operations at `k` of fibers declaring `k`. This is precisely the pair of requirements §3.2.2 left open: the provider's binding stays readable for the whole of a dependent's life, *including its teardown*, and the withdrawal is deferred until the dependents are gone.
* **Theorem 73 (Progress)**: assuming `≺` acyclic, bounded iterator length, and finitely many names, the system quiesces — the guard always releases.
* **Theorem 80 (Confluence)**: any two step sequences taking the same orchestration steps reach quiescent states related by `≃`/`≃_K` (after a renaming), with a canonical form that orders episodes by `⊲`. Requires every component to be **total on its provision** (Definition 76: an activation that finishes installs every key it declares) and excludes failed fibers. This is the theorem the loader's incremental reconciliation rests on.
* **Confinement** (Definition 55) is the per-component discipline that makes the whole thing compositional: an effect function confined to `n` (1) changes no fiber's presence, changes other fibers only in `σ_m|_{d_n}`, and itself only in `σ_n`; (2) can distinguish states only by `σ_n` and `σ_m|_{d_n}`. Definition 56 fixes the allowed forms; Lemma 57 derives confinement from them.
* The paper's own summary of the division of labour [§3.4.2]: *"The commuting part is carried by the effects… The order-sensitive part is carried by the coeffects, since a key whose operations do not commute is one whose order has to be imposed from outside the effects."* Two places impose that order: **within** a component, the accumulator (LIFO); **across** components, a declared coeffect (provider precedes satisfied consumer).

**Practical discipline for a Rust port** (derived from the above):
1. Route every mutation through the context; return an inverse.
2. Touch only keys you declared (`d`) or provide (`p`); never read another fiber's control state.
3. Publish at each key a *commutative* operation set: give every registration a unique tag and make the inverse remove exactly that tag. If an interface has order-sensitive semantics, either make the order internal to the provider or expose it as a key whose *declared* usage is sequential.
4. Do not expose outcomes (handles, indices) that make two orders distinguishable unless callers need them.
5. Because the witness is unchecked (§2.2), make this an explicit documented/`unsafe` contract, or generate the accessors with a macro so the obligation is discharged mechanically (the paper points at Rust traits and proc macros for exactly this at §6.4).

---

## 10. Suggested Rust design shape (prose only — no code)

The following is a language-neutral sketch of what the mechanisms above require; it is deliberately not Rust code.

* **`Context`** — a cheaply-cloneable handle (`Arc`) to `{ parent: Option<ContextHandle>, fiber: FiberHandle, isolate: RealmTable, intercept: MetadataTable }`, with shared per-root tables `{ store: HashMap<RealmSymbol, ServiceImpl>, props: HashMap<Key, Property> }` and `{ registry: HashMap<PluginId, PluginRuntime> }`. `extend`/`isolate`/`intercept` return a new handle sharing the parent chain. All service access is an explicit method on this handle (`get`, `set`, `provide`) rather than a proxy trap; the "declared before usable" check moves into the accessor generated per dependency.
* **Effects** — `trait Effect { fn run(self: Box<Self>) -> EffectOutcome }` is the wrong shape; the code's shape is: a callback that *returns* a disposer, and an async-iterator form for multi-stage loads. Model it as an `Effect` enum (Ready(fn), Async(future-of-fn), Iter/AsyncIter) plus `Fiber::effect(callback) -> Disposer`, with the disposer list stored per fiber **and** per call, LIFO within a call, sequential within a call, concurrent across a fiber's top-level effects. A `guard: Arc<AtomicBool>`-style epoch gives idempotence and the step-boundary abort.
* **Coeffects** — `Spec = BTreeSet<Key>`; the *target digest* is a `Vec<ProviderId>` (not a value), recomputed from the committed views of the ACTIVE providers; transitions are driven by comparing digests; a monotonic `u64` counter supplies `ProviderId`.
* **Lifecycle** — a state enum `{ Pending, Loading, Active, Failed, Disposed, Unloading }` plus `inertia: Option<JoinHandle>` and a two-phase `reload`/`unload` pair that re-checks the target at completion and at each iteration boundary. `Failed` must be sticky and cleared only by an explicit `update`.
* **Services** — one provider per key per realm, enforced by an error; `provide` returns a disposer that first removes the binding, then notifies and *awaits* dependents, then clears its own self-view. `set` is same-provider-only and silent; a `Service::check` hook lets a provider veto its own availability dynamically.
* **Loader** — an entry record with `{ id, name, config, group, disabled, inject, intercept, isolate }`, an id-keyed diff per group, per-field dispatch (patch context always; fiber update only on `config`/group), and a `commit(EntryChange)` funnel so a persistence layer can be a separate crate.
* **Events** — registration as an effect (listener removed on unload), five dispatch modes with the exact bail predicate and waterfall `next`-once rule, and realm-filtered dispatch. Emission is untracked.

---

## 11. Quick reference: exact names

**Core methods** — `ctx.effect(cb, label?)`, `ctx.plugin(plugin, ...args?)`, `ctx.inject(deps, cb)`, `ctx.get(name, strict?)`, `ctx.set(name, value)`, `ctx.provide(name, value?, check?)`, `ctx.accessor(name, options)`, `ctx.mixin(source, keys)`, `ctx.isolate(name, label?)`, `ctx.intercept(name, config)`, `ctx.extend(meta)`, `ctx.on/once/emit/parallel/serial/bail/waterfall`, `ctx.root`, `ctx.baseUrl`, `ctx.fiber`, `ctx.events/logger/reflect/registry/loader`.

**Fiber** — `uid`, `ctx`, `config`, `state`, `store`, `inertia`, `inject`, `runtime`, `parent`, `dispose`, `effect()`, `getEffects()`, `assertActive()`, `await()`, `restart()`, `update(config, noSave?)`, `_refresh()`, `_checkImpl(name)`, `_updateState(cb)`, `_reload()`, `_unload()`, `name`.

**Reflect** — `store`, `props`, `get`, `_getImpl(name, strict)`, `set`, `provide`, `notify(names, filter?)`, `accessor`, `mixin`, `trace`, `bind`, `static handler`.

**Registry** — `counter`, `size`, `resolve(plugin)`, `get/has/delete/keys/values/entries/forEach`, `inject`, `plugin`; `Plugin.{Base,Function,Constructor,Object,Transform,Runtime}`; `Inject`, `Inject.resolve`, `InjectKey`.

**Symbols** — `Context.effect/filter/isolate/intercept/is`; `Service.init/check/config/invoke/extend/tracker/resolveConfig`; internal `shadow/caller/receiver/original/metadata/initHooks/checkProto`.

**Fiber states** — `PENDING, LOADING, ACTIVE, FAILED, DISPOSED, UNLOADING` (`PENDING` ≙ paper `Inactive`, `LOADING` ≙ `Reloading`).

**Loader** — `Entry`, `EntryGroup`, `Group`, `EntryTree`, `EntryOptions`, `EntryChange`, `Loader`, `Loader.Config {baseUrl}`, `Loader.Intercept {await}`, `EntryTree.sep = ':'`, `Entry.key`, `EntryGroup.key`, `sortKeys`.

**Loader events** — `exit`, `loader/config-update`, `loader/entry-init`, `loader/partial-dispose`, `loader/patch-context`.
**Kernel events** — `internal/plugin`, `internal/status`, `internal/service`, `internal/update`, `internal/get`, `internal/set`, `internal/listener`, `internal/dispatch`.
**HMR events** — `hmr/change`, `hmr/reload`.

**Error types / messages** — `CordisError('INACTIVE_EFFECT')` = `"cannot create effect on inactive context"`; `ValidationError` = `"invalid config:\n  - …"`; `"invalid plugin, expect function or object with an \"apply\" method, received <t>"`; `'invalid effect'`; `"cannot get property \"x\" without inject"`; `"cannot get required service \"x\" in inactive context"`; `"cannot set property \"x\" without provide"`; `"cannot set property \"x\" in multiple fibers"`; `"service \"x\" has been registered at <fiber>"`; `'next() called multiple times'`; `"cannot resolve entry <id>"`; `"entry <id> is not a group"`; `ConfigFileError` (`failed to <read|parse|validate> config file <path>`); `StaleWriteError`.

---

## 12. Paper ↔ code divergences (get these right in a port)

1. **`ctx.use` (paper Alg. 4, Table 2) is `ctx.plugin()` in the code**; `ctx.inject` is its sugar. **`ctx.using` does not exist** in this revision.
2. **`ctx.set` semantics differ, and this is the subtlest trap.** Paper Alg. 2's `set(k,v)` installs a binding *and calls `notify` on install and removal*, and it is typed `𝔈*_Σ` (an effect function). In the code, the tracked/notifying operation is **`ctx.provide()`**, while **`ctx.set()` (and `ctx.x = v`)** only mutates the value of a binding *this same fiber already provided*, returns `true`, and **does not notify** — so a value-only change is invisible to dependents. Porting Alg. 2 as `set` would make in-place updates observable and break the "provider replacement must withdraw-then-reinstall" behaviour that the target-view digest depends on.
3. **Entry fields**: paper `{id, url, isolate, intercept, config, disabled}`; code `EntryOptions = {id, name, config, group, disabled, inject}` **plus** `isolate`/`intercept` added by the isolate plugin's module augmentation. `name` ≙ `url`.
4. **HMR rollback**: paper Alg. 10 describes a fully transactional reload (back up caches, restore and rebuild every stale entry from backup on failure). The code rolls back **only the module caches** in the re-import stage (stage 1); the fiber-swap stage has **no rollback** and explicitly leaves a failed plugin failed: *"No rollback: a plugin that fails to load is left failed, exactly as it would be on a cold start."*
5. **Lifecycle state count**: paper Θ has 4 states; the code has **6** (`PENDING` ≙ `Inactive`, `LOADING` ≙ `Reloading`, plus `FAILED` and `DISPOSED`, which the paper treats as §4.4 extensions / absence).
6. **Field names**: paper's `fiber.apply` → `fiber.runtime.callback` + `resolveConfig`; paper's `fiber.committed` → `fiber.store`; paper's `fiber.target` → `fiber._runner.epoch` recomputed by `_refresh()`; paper says `fiber.dispose` is the accumulator, whereas in code `fiber.dispose` is the closure that drains `inertia` and `fiber._disposables` is the accumulator.
7. **`Service.check` has no paper counterpart.** The paper's `target_n` reads only `τ_n` and satisfaction; the code additionally lets a provider dynamically veto its own availability (`ctx.reflect.provide(name, value, check)`), which the `Loader` uses for its `await` option. This is a genuine extension, not a formalization.
8. **Root access escape hatch.** The Proxy falls back to `ctx.reflect.get(prop, false)` (unchecked, non-throwing) when the def site is a fiber-less context, and to `Reflect.set` when an unknown property is set on a fiber-less context. The paper's Algorithm 6 has no such case; the code needs it for the root/CLI context.
9. **Paper Table 2 is unusable as extracted.** The two columns come out of `pdftotext -layout` in separate blocks with mismatched row counts; treat the mappings in §5.2 of this report (reconstructed from the algorithms and prose) as authoritative.
10. **The `Listener`/event subsystem is outside the calculus** but has *derived realization* per Definition 23. Nothing in the paper specifies dispatch modes, bail semantics, or waterfall `next`-once; these are engineering, and should be ported from the code.

---

## 13. Explicitly unverified / open

* **[I]** Whether a Rust `Context` should keep the def-site/use-site distinction. The paper's Definition 56 confines an effect function to keys `d_n ∪ p_n` and the calculus never needs a shadow pair; the code needs it because JS resolves `ctx.foo` lazily. My reading is that a Rust port can drop the *mechanism* and keep the *discipline* (explicit context argument + macro-generated typed accessors), but this is an inference, not something either source states.
* **Not read**, so not claimed: `packages/create/**`, `packages/core/bin.js`, `packages/logger-console/**`, `packages/timer/**` (the CLI bootstrap that mounts `Loader` + `Include` end to end); the middle of `Hmr.analyzeChanges` beyond the fixpoint sketch; the body of `hmr/src/error.ts`. The `Include` journal/patch module was read at the declaration level plus its call sites and the `Loader`-side hooks.
* The paper is a **preprint under active revision** (its own README says so), and the implementation is `4.0.0-rc.10` with an explicit "API is not yet stable and may change without notice" banner. Pin a commit for any port and expect drift.
* I did not find any `cordis.js.org` documentation site; the only docs are the harness's `cordis-primer` (Chinese, and partly harness-specific) and the two READMEs.
