# 07 — MUGEN Trigger Expression Evaluator Semantics

A faithful specification of the numeric and evaluation semantics of M.U.G.E.N trigger
expressions, written to drive the `fp-vm` reimplementation (task 4.4) and resolve backlog
**CB4** (lexer integer-overflow behavior). Each section maps to one research item.

The two load-bearing sources are:

1. **Elecbyte's official CNS reference** — `elecbyte.com/mugendocs/cns.html`, the "Expressions"
   section. This is the documented *contract*.
2. **Ikemen GO** (MIT-licensed open-source successor) — `src/bytecode.go` and `src/compiler.go`.
   This is the *de-facto behavior* the entire community content corpus has been validated
   against for years. Where the doc is silent or ambiguous, Ikemen GO is treated as ground truth.

All Ikemen GO line numbers below are from a `--depth 1` clone of `ikemen-engine/Ikemen-GO`
taken 2026-06-13. They will drift across versions; the surrounding function names are the
durable references.

> **Clean-room note:** This document was written from the *documented format* (Elecbyte docs)
> and the *MIT-licensed* Ikemen GO source. No closed Elecbyte source was consulted. Ikemen GO
> is MIT — its algorithms may be referenced and reimplemented.

---

## 1. Type system

MUGEN expression values are one of **three** dynamic types (cns.html, "Expressions"):

> "MUGEN uses three data types: 32-bit integers, 32-bit floats, and a special null value,
> 'bottom'."

- **int** — 32-bit signed (`i32`). Literals have no decimal point: `7`, `-3`, `0`.
- **float** — single-precision (`f32`). Literals require a decimal point: `7.0`, `.5`, `3.14`.
  Note: a bare `7` is an **int**, never a float; `7.` / `.7` are floats.
- **bottom** — a null/error sentinel. "Bottom zeros out any expression that it appears in
  (with a few very limited exceptions)." It is produced by illegal operations (divide-by-zero,
  `ln(0)`, invalid exponentiation, etc.) and propagates.

**How an expression's type is decided** — types are *not* declared; each operator decides its
result type from its operand types at runtime:

- A literal carries its lexical type (int vs float).
- Arithmetic on two ints yields int; any float operand promotes the result to float (see §2).
- Comparisons, logical ops, and bitwise ops always yield **int** (1/0 or a bit pattern), §5.

**Mixed int/float promotion** — when one operand is int and the other float, the int is promoted
to float and the operation is done in float. Ikemen GO encodes this with an `iota` ordering trick:

```go
// bytecode.go ~75
const ( VT_None ValueType = iota; VT_Float; VT_Int; VT_Bool; ... )
// VT_Float (1) < VT_Int (2), so the *minimum* vtype is Float iff either operand is float:
if ValueType(Min(int32(v1.vtype), int32(v2.vtype))) == VT_Float { /* float path */ } else { /* int path */ }
```
This `Min(vtype) == VT_Float` test is the canonical "either operand is float ⇒ promote" rule and
appears in every binary arithmetic/comparison op (`add`, `sub`, `mul`, `div`, `mul`, `gt`, `eq`, …).

**Ikemen GO storage detail (important for fp-vm):** Ikemen stores *every* value as a Go
`float64` with a `vtype` tag (`BytecodeValue{ vtype, value float64 }`, bytecode.go ~1080). Ints
are the float64 bit-exact representation of the i32. Conversions `ToI() = int32(value)` and
`ToF() = float32(value)` are applied on demand. fp-vm may instead use a proper tagged union
(`enum { Int(i32), Float(f32), Bottom }`) — this is cleaner and avoids float64↔i32 round-trip
hazards, **provided** the narrowing/saturation rules in §4 are matched exactly.

`bottom` in fp-vm: model as an explicit `Bottom`/`Undefined` variant. (Ikemen calls it
`VT_Undefined` and represents it as `NaN`; its `ToI()`/`ToF()` of undefined return `0`,
and a *condition-type trigger* treats bottom as `0` — "an expression that generates an error
will never cause a trigger to fire.")

---

## 2. Division (`/`) and modulo (`%`)

**Division — type-dependent (cns.html):**

> "If x and y are both ints, then x/y gives the integer quotient." (`7/2` → `3`)
> "If x and y are both floats, then x/y returns a float." (`7.0/2.0` → `3.5`)
> Mixed int/float promotes to float division. "Division by 0 will produce bottom."

Integer division is **truncating toward zero** (C semantics, which Go's `int32 / int32` also
gives): `-7/2 == -3`, `7/-2 == -3`.

Ikemen GO (`bytecode.go` `div`):
```go
if v2.ToF() == 0 { *v1 = BytecodeUndefined(); printBytecodeError("Division by 0") }
else if Min(vtype)==VT_Float { v1.SetF(v1.ToF() / v2.ToF()) }   // f32 division
else { v1.SetI(v1.ToI() / v2.ToI()) }                            // i32 trunc-toward-zero
```
Note Ikemen guards divide-by-zero on the *float* value of the divisor (`v2.ToF()==0`), so `x/0`
and `x/0.0` both → bottom.

**Modulo — int-only (cns.html):**

> "If x and y are both ints, then x % y gives the remainder when x is divided by y. If one or
> both are floats, or if y is 0, then bottom is produced."

So `%` is **defined only for two ints**. Sign follows C truncated division: the result takes the
sign of the **dividend** (`-7 % 3 == -1`, `7 % -3 == 1`), matching Go's `%`.

Ikemen GO (`bytecode.go` `mod`) — note a *divergence* worth flagging:
```go
if v2.ToI() == 0 { *v1 = BytecodeUndefined(); printBytecodeError("Modulus by 0") }
else { v1.SetI(v1.ToI() % v2.ToI()) }   // uses ToI() — it does NOT bottom on float operands
```
Ikemen **coerces float operands to int** rather than producing bottom (it truncates `7.9 % 2` to
`7 % 2`). The Elecbyte doc says float operands should yield bottom. **fp-vm decision:** follow
the documented Elecbyte behavior (float operand ⇒ bottom) for spec-faithfulness, but this is a
low-impact edge; content rarely uses `%` on floats. Either is defensible — document the choice.
Divide-by-zero on `%` → bottom (both agree).

---

## 3. Exponentiation (`**`)

**Result type & overflow (cns.html):**

> For non-negative integer exponents, `x**y` produces an **int** result, but "it is very easy to
> overflow... In these cases MAX_INT (the largest possible integer) will be returned, and a
> warning will be generated."
> For negative or fractional exponents: "both arguments are promoted to float and x**y is
> computed as an exponentiation of real numbers. An invalid exponentiation like `-1 ** .5` will
> produce bottom."

So:
- `int ** non-negative int` → **int** (via repeated squaring), **saturating to MAX_INT** on
  overflow + warning.
- any float operand, **or** a negative exponent → **float**, real `pow`. Invalid (e.g.
  `(-1) ** 0.5` = NaN) → **bottom**.

**Associativity: right-to-left.** `**` binds tighter than `* / %` and, like unary ops, associates
right. (cns.html: precedence "same as in C", unary and assignment right-associative; Ikemen's
`expPow` recurses, see §9.)

Ikemen GO (`bytecode.go` `pow`):
```go
if Min(vtype)==VT_Float || v2.ToF() < 0 {     // float path: any float OR negative exponent
    v1.SetF(Pow(v1.ToF(), v2.ToF()))           // math.Pow; NaN/Inf result -> "Invalid exponentiation"
} else {                                       // int path: binary exponentiation by squaring
    ... loop multiplying tmp ...               // overflow wraps in i32 here in Ikemen, see note
}
```
**Note / surprise:** Ikemen's *int* power path does the repeated-squaring in plain `int32`
multiplications, which **wrap** (two's complement) rather than saturating to MAX_INT as the doc
promises. The doc says saturate. fp-vm should prefer the **documented** MAX_INT saturation for
the int path (detect overflow, clamp, warn) — it is more predictable and matches Elecbyte's
stated contract. (Ikemen also carries a `WinMugen`-vs-`1.1` quirk for `0 ** -1`; not worth
reproducing — treat `0 ** negative` as bottom/infinity-error.)

---

## 4. Integer width & overflow  →  **CB4 decision**

MUGEN ints are **32-bit signed**. The question CB4 asks: when a value exceeds i32 range, does
the engine **wrap** (two's complement) or **saturate** (clamp to MIN/MAX)? And does fp-vm's
lexer (currently "overflow → 0") need to change?

**Answer: SATURATE.** Both the literal lexer path *and* the runtime narrowing path in Ikemen GO
saturate to the i32 bounds. Evidence (empirically run against the cloned source's helpers):

**(a) Literal lexing — `Atoi` (`common.go` ~171):**
```go
func Atoi(str string) int32 {
    var n int64
    ... for each digit ...
        n = n*10 + digit
        if n > 2147483647 {
            // WARNING: Atoi conversion outside int32 range
            if str[0]=='-' { return IErr }   // IErr = MinInt32
            return IMax                       // IMax = MaxInt32
        }
    ...
}
// IMax = int32(^uint32(0) >> 1) = 2147483647 ;  IErr = ^IMax = -2147483648
```
A literal larger than i32 → **MaxInt32** (or MinInt32 if negative), with a warning. **Not 0.**

**(b) Expression-literal parse — `number()` (`compiler.go` ~696):**
```go
f, _ := strconv.ParseFloat(token, 64)
...
if f > math.MaxInt32 { return {VT_Int, MaxInt32} }
if f < math.MinInt32 { return {VT_Int, MinInt32} }
return {VT_Int, f}
```
Same saturation, applied to the parsed-as-float64 magnitude.

**(c) Runtime narrowing — `ToI()` does `int32(value)` on a `float64`.** Go's `float64 → int32`
conversion for out-of-range magnitudes is implementation-defined by the Go spec, but on the
mainstream amd64/arm64 toolchains it **saturates**, and Ikemen relies on this. Verified by
running Go locally:
```
int32(2147483648.0)   == 2147483647    // +overflow saturates to MaxInt32
int32(4294967296.0)   == 2147483647
int32(-2147483649.0)  == -2147483648   // -overflow saturates to MinInt32
int32(+Inf)           == 2147483647
int32(NaN via ToI)    == 0             // Ikemen special-cases NaN/undefined -> 0
int32(3.9)            == 3 ; int32(-3.9) == -3   // truncate toward zero
```
So at runtime, a float result narrowed to int (e.g. via `floor`, `ceil`, or `var() :=` of a huge
float) clamps to the i32 bounds, and NaN/bottom → 0.

**(d) Arithmetic overflow at runtime** is a *different* case. Ikemen's `add/sub/mul` int paths
use native `int32` arithmetic (`v1.ToI() * v2.ToI()`), which **wraps** (two's complement) — e.g.
`2000000000 + 2000000000` wraps negative. This is standard hardware behavior and content depends
on it in rare cases (hash-like tricks). So the precise picture is:

| Path | Behavior |
|------|----------|
| **Literal too big for i32** (lexer) | **Saturate** to MIN/MAX i32 + warn |
| **float→int narrowing** (`floor`/`ceil`/`ToI`, `**` int-overflow per doc) | **Saturate** to MIN/MAX i32; NaN/bottom → 0 |
| **int `+ - *` arithmetic overflow** | **Wrap** (two's complement, native i32) |
| **`**` int-power overflow** | doc says **saturate to MAX_INT**; Ikemen wraps — prefer **saturate** |

**CB4 recommendation (concrete):**
1. **Change the lexer's `overflow → 0` to `overflow → saturate`** (i32::MAX for positive,
   i32::MIN for negative), emitting a `tracing::warn!`. This matches both Elecbyte's documented
   `Atoi`/`number()` behavior and avoids the silent-zero footgun that `→ 0` creates (a clamped
   stat reads as a plausible large value, not a silent 0).
2. **Narrowing from float to int in the evaluator must saturate**, and **NaN/bottom must narrow
   to 0.** Implement as: `if x.is_nan() {0} else {x.clamp(i32::MIN as f32, i32::MAX as f32) as i32}`.
   Do **not** use Rust's `as i32` directly on an out-of-range `f32` without the clamp — Rust `as`
   saturates too (since 1.45) so `x as i32` is actually safe in Rust, but be explicit about NaN→0
   because Rust gives `NaN as i32 == 0`, which happens to match. (Confirm with a unit test.)
3. **Keep wrapping for int `+ - *` arithmetic** — use Rust `wrapping_add/wrapping_sub/wrapping_mul`
   so the native two's-complement behavior is reproduced deterministically across platforms
   (do *not* let it panic in debug builds via the default `+`).
4. **`**` int-power: saturate to MAX_INT on overflow** (per the documented contract), detecting
   overflow during the squaring loop.

---

## 5. Booleans & comparisons

- **Comparisons return int 1/0.** All of `< <= > >= = !=` produce int `1` (true) or `0` (false)
  (cns.html). Operands are promoted to float if either is float (§1), else compared as i32.
  (Ikemen `gt/ge/lt/le/eq/ne`, each `SetB(...)` → stored as int 0/1.)
- **What counts as "true": any nonzero value.** This includes negative ints and nonzero floats.
  `! x` is `1` iff `x == 0`, else `0` (cns.html). Ikemen `ToB()`: `value != 0 && !undefined`.
  (Subtle: a tiny nonzero float like `0.0001` is true; exactly `0.0` / `-0.0` is false.)
- **Logical ops coerce to bool, return int 1/0.** `&&`, `||`, `^^`, `!` all map operands through
  the "nonzero ⇒ true" rule and yield int 0/1 (Ikemen `bland`/`blor`/`blxor`/`blnot` use `ToB()`
  then `SetB`). They **do** therefore "coerce floats" (via nonzero test). They are *not*
  bitwise — those are the separate `& | ^ ~` operators (§9), which require ints and bottom on
  floats.
- **Short-circuit: YES for `&&` and `||`.** Ikemen compiles them with conditional jumps:
  `&&` emits `OC_jz` (jump-if-zero, skipping the RHS) and `||` emits `OC_jnz`
  (`compiler.go` `expBoolAnd`/`expBoolOr`, ~5688/5733). So the RHS is not evaluated when the LHS
  already decides the result — important because the RHS may contain a redirect (`enemy,...`)
  that would error/bottom. fp-vm **must** short-circuit to match (both for behavior and to avoid
  spurious bottoms). `^^` (logical XOR) cannot short-circuit — both sides always evaluated.

---

## 6. Range / interval comparisons

Syntax (cns.html): an interval may appear **only** on the right of `=` or `!=`.

| Form | Meaning |
|------|---------|
| `x = [a,b]`  | `(x >= a) && (x <= b)` — inclusive both ends |
| `x = (a,b)`  | `(x >  a) && (x <  b)` — exclusive both ends |
| `x = [a,b)`  | `(x >= a) && (x <  b)` — half-open |
| `x = (a,b]`  | `(x >  a) && (x <= b)` — half-open |
| `x != [a,b]` | `(x <  a) \|\| (x >  b)` — the negation of `=[a,b]` |
| `x != (a,b)` | `(x <= a) \|\| (x >= b)` |
| `x != [a,b)` / `x != (a,b]` | negations of the corresponding `=` forms |

> cns.html: "x = [y,z] is equivalent to (x >= y) && (x <= z). … x != [y,z] is equivalent … to
> (x < y) || (x > z)."

**Yes — `!=` negates membership.** `x != [a,b]` is exactly `!(x = [a,b])` (De Morgan applied to
the bound checks).

**Type rule:** the three values `x`, `a`, `b` are jointly type-checked; if **any** is float, all
three compared as float, else as int. (Ikemen `rangeCheck`, `bytecode.go` ~1525, uses
`Min(Min(vtype_x, vtype_a), vtype_b) == VT_Float`.) Each of the four endpoint behaviors is keyed
by an opcode: `OC_range_ii` (`[]`), `OC_range_ie` (`[)`), `OC_range_ei` (`(]`), `OC_range_ee`
(`()`), selected in `compiler.go` `expRange` ~5517.

**bottom handling:** `rangeCheck` *does* propagate undefined (if `x`/`a`/`b` is bottom → bottom),
which Ikemen notes is inconsistent with `<`/`>` (which don't) — a documented quirk. fp-vm: make
range checks bottom-propagating to match.

**Restriction:** "no operator symbols other than `=` or `!=` may appear before an interval" — so
`5 > [0,2]` and `4 + [1,4)` are syntax errors. The interval must be the rightmost term of its
(sub)expression. The lexer/parser must reject intervals elsewhere.

---

## 7. Comparison chaining

The Elecbyte doc does not bless `a <= x <= b`; the *intended* idiom is the range form
(`x = [a,b]`) or explicit `&&`. **However**, Ikemen GO's parser does **not** reject chained
relational operators — `expGrls` (`compiler.go` ~5447) loops over `> >= < <=` **left-
associatively**, so `a < b < c` parses as `(a < b) < c`: first `a < b` yields int `0`/`1`, then
that `0`/`1` is compared `< c`. This is almost never what an author means and is a classic bug
source, but it **does not error** — it silently computes the left-associative chain.

**fp-vm decision:** match Ikemen — allow it to parse (left-associative, same as C's relational
chaining), since rejecting it would break any content that accidentally relies on the silent
behavior. Optionally emit a `tracing::warn!` on a detected relational chain (`(cmp) cmp value`)
as a content-lint, but still evaluate it left-associatively. Do **not** implement Python-style
chained comparison (`a < b < c` ⇒ `a<b && b<c`) — that is wrong for MUGEN.

---

## 8. Built-in functions — return types & rounding

From cns.html (trigger.html lists the functions) corroborated by Ikemen `bytecode.go`:

| Function | Arg | Result type | Notes |
|----------|-----|-------------|-------|
| `floor(x)` | float | **int** | `math.Floor` then narrow to i32. **Type-preserving on int:** if arg is already int, Ikemen leaves it unchanged (no-op). NaN → bottom. |
| `ceil(x)`  | float | **int** | `math.Ceil` then narrow. Int arg passes through unchanged. NaN → bottom. |
| `abs(x)`   | int or float | **same as input (type-preserving)** | int→int (`Abs` i32), float→float (`math.Abs`). The one truly type-preserving unary. |
| `min(a,b)` | any | **float (in Ikemen)** | ⚠️ Ikemen `min`/`max` always `SetF` → **float result even for two int args**. The doc implies type-preserving. **fp-vm decision:** make `min`/`max` **type-preserving** (int,int→int; any float→float) — this matches author expectation and the doc better than Ikemen's float-always quirk. Flag as an intentional, documented divergence. |
| `max(a,b)` | any | float (Ikemen) / type-preserving (recommended) | same as `min`. |
| `sin/cos/tan(x)` | any | **float** | radians; `math.Sin/Cos/Tan` → f32. |
| `asin/acos/atan(x)` | any | **float** | radians. `atan` is single-arg (no `atan2` in classic MUGEN; Ikemen adds `atan2`). |
| `exp(x)` | any | **float** | e^x. |
| `ln(x)` | any | **float** | natural log; `x <= 0` → **bottom** + warning. |
| `log(b,x)` | any | **float** | `ln(x)/ln(b)`; either arg `<= 0` → bottom. (Note arg order: base first in MUGEN.) |
| `random` | (no parened args in classic MUGEN; `Random` trigger) | **int** | returns `[0,999]` in classic MUGEN. Ikemen also exposes `Rand`/range forms returning int via the deterministic LCG (§11). Inclusive integer range. |
| `cond(c,t,f)` / `ifelse(c,t,f)` | — | type of selected branch | ternary; selects `t` if `c` nonzero else `f`. (`cond` short-circuits the unused branch.) |

Constants: `pi` and `e` are float literals (`compiler.go` ~4088).

**Rounding summary:** `floor` = toward −∞, `ceil` = toward +∞, int division & int narrowing =
**toward zero** (truncation), `**` int-power exact then saturate. There is no banker's rounding.
Ikemen also has a `round(x, n)` helper using `floor(x*10^n + 0.5)/10^n` (round-half-up) but that
is an Ikemen extension, not classic MUGEN.

---

## 9. Operator precedence & associativity (full table)

From cns.html ("precedence is basically the same as in C … evaluated from left to right, except
for the unary operators and the assignment operator, which associate right to left") and
**validated against Ikemen GO's recursive-descent chain** in `compiler.go`. The compiler's call
chain (lowest-precedence parser calls the next-higher) is, top (loosest) to bottom (tightest):

```
expBoolOr (||)  →  expBoolXor (^^)  →  expBoolAnd (&&)  →  expOr (|)  →  expXor (^)
  →  expAnd (&)  →  expEqne (= != intervals)  →  expGrls (> >= < <=)  →  expAdsb (+ -)
  →  expMldv (* / %)  →  expPow (**)  →  expPostNot (unary ! ~ -)  →  expValue (atoms)
```

Yielding this precedence table (**highest binds first** at top):

| Lvl | Operators | Assoc | Result type |
|----:|-----------|-------|-------------|
| 1 (tightest) | `()` grouping, function calls, atoms | — | — |
| 2 | unary `!`  `~`  `-` (and unary `+`) | **right** | `!`,`~` → int; `-` preserves type |
| 3 | `**` | **right** | int or float (§3) |
| 4 | `*`  `/`  `%` | left | §2 |
| 5 | `+`  `-` (binary) | left | int or float |
| 6 | `>`  `>=`  `<`  `<=` | left (chains! §7) | int 0/1 |
| 7 | `=`  `!=`  and intervals `[..]`/`(..)` | left | int 0/1 |
| 8 | `&`  (bitwise AND) | left | int (bottom on float) |
| 9 | `^`  (bitwise XOR) | left | int (bottom on float) |
| 10 | `\|` (bitwise OR) | left | int (bottom on float) |
| 11 | `&&` (logical AND) | left, **short-circuit** | int 0/1 |
| 12 | `^^` (logical XOR) | left | int 0/1 |
| 13 (loosest) | `\|\|` (logical OR) | left, **short-circuit** | int 0/1 |

**Where is `:=`?** The Elecbyte precedence list places assignment near the bottom (right-assoc),
but Ikemen does **not** parse `:=` as a general infix operator in the precedence chain — it is
recognized specially when the left side is a bare `var(n)`/`fvar(n)`/`sysvar(n)`/`sysfvar(n)`
(`compiler.go` `_var`, ~1289). See §10. Treat `:=` as the lowest-precedence, right-associative
form, but parse it only in the var-assignment context.

**Surprises vs C:**
- **`**` exists** (C has no exponent operator) and is right-associative and **higher** than `*`/`/`.
- **`^^` (logical XOR) and intervals `[a,b]`** have no C equivalent.
- **Bitwise `& ^ |` sit *below* equality** (levels 8–10, above the logical ops) — this *matches*
  C's notoriously-low bitwise precedence, so `a = b & c` parses as `(a = b) & c` here too. Keep this.
- **Relational ops chain** (§7) — like C, unlike Python.
- `~` and `&`/`|`/`^` require **int** operands; a float operand → **bottom** (cns.html;
  Ikemen `and`/`or`/`xor`/`not` go through `ToI()`).

---

## 10. Assignment `:=` in expressions

`var(n) := expr` (and `fvar`, `sysvar`, `sysfvar`) is a valid **expression** that performs the
assignment **and returns the assigned value** (cns.html: an unredirected `var(n)`/`fvar(n)` must
be on the left; the value is converted to the variable's type).

Confirmed in Ikemen GO:
- Parse: `compiler.go` `_var` (~1289) — on seeing `:=`, it compiles the RHS (`expEqne`), emits
  `OC_st_` + `OC_st_var`/`OC_st_fvar`/`OC_st_sysvar`/`OC_st_sysfvar`.
- Eval: `bytecode.go` `OC_st_var` (~2389) writes the var then `*sys.bcStack.Top() = c.varSet(...)`,
  i.e. **leaves the stored value on the stack** as the expression's result.
- `varSet` (`char.go` ~7943) comment: *"We also return the value because var assignment can be
  used in expressions."* Return type follows the var: `var()`/`sysvar()` store/return **int**
  (`varSet` truncates the RHS via `value.ToI()`), `fvar()`/`sysfvar()` store/return **float**
  (`value.ToF()`). Negative index → warning + bottom.

**Evaluation order:** RHS is evaluated first (its bytecode runs before the store opcode), then the
store happens, then the stored value remains as the result. For `var(0) := var(0) + 1`, the read
of `var(0)` on the RHS sees the *old* value, consistent with strict left-to-right then store.

**fp-vm:** support `:=` only with a `var/fvar/sysvar/sysfvar` LHS; truncate-to-int for int vars,
to-f32 for float vars; push the stored value as the result. Right-associative (`a := b := c`).

---

## 11. Determinism notes (replay / rollback)

For frame-perfect netplay rollback and replay, the evaluator must be **bit-deterministic** across
runs and machines. Key points:

- **Random is a deterministic LCG, not OS rand.** Ikemen uses the Park–Miller "minimal standard"
  generator on a single `int32 sys.randseed` (`common.go` `Random`):
  ```go
  w := sys.randseed / 127773
  sys.randseed = (sys.randseed - w*127773)*16807 - w*2836
  if sys.randseed <= 0 { sys.randseed += IMax - Btoi(sys.randseed==0) }
  ```
  `RandI(x,y)` derives an inclusive integer range from it. fp-vm **must** use this exact LCG
  (constants 16807 / 127773 / 2836) and treat `randseed` as part of saved/rolled-back game state.
  Seed only once at match start (Ikemen seeds from `time.Now().UnixNano()` then advances purely
  deterministically). For replays, persist the initial seed.

- **Float order matters.** Use **f32** (single precision) for float values to match MUGEN, and
  fix the evaluation order (the precedence/associativity above) so the same expression yields the
  same f32 bits everywhere. Avoid f64 intermediate accumulation in places MUGEN would use f32
  (Ikemen stores as f64 internally but narrows via `float32(...)` at each `SetF`, effectively
  rounding to f32 after every op — replicate that: **round to f32 after each float operation**).

- **Transcendentals are a cross-platform hazard.** `sin/cos/tan/exp/ln/pow` are not guaranteed
  bit-identical across libm implementations. For deterministic rollback, either (a) ship a single
  fixed implementation of these (a vendored, version-pinned math routine) used on all platforms,
  or (b) ensure both peers run the identical binary. Document this as a determinism requirement.

- **NaN/bottom is deterministic.** Funnel all error cases to the single `Bottom` value with the
  fixed narrowing rules (NaN→0 for int, →0 effective for conditions) so an error path can't
  diverge between peers.

---

## Decisions for fp-vm (summary)

1. **Value model:** tagged union `Int(i32) | Float(f32) | Bottom`. Promotion: any float operand ⇒
   float result; comparisons/logical/bitwise ⇒ int. (§1)
2. **`/`:** int/int → truncating-toward-zero i32 division; any float → f32 division; `/0` → Bottom.
   **`%`:** int-only remainder (sign of dividend); float operand → Bottom (per doc, diverging from
   Ikemen's coerce-to-int); `%0` → Bottom. (§2)
3. **`**`:** right-associative; int^nonneg-int → int with **saturate-to-MAX_INT on overflow** (per
   doc, not Ikemen's wrap); any float or negative exponent → f32 `pow`; invalid → Bottom. (§3)
4. **CB4 — SATURATE, not wrap, not zero:**
   - Lexer: **change `overflow → 0` to `overflow → saturate`** (i32::MAX / i32::MIN) + `warn!`.
   - float→int narrowing: **saturate**, NaN/Bottom → 0 (Rust `as i32` already saturates; add an
     explicit NaN→0 test).
   - int `+ - *` arithmetic: **wrap** (use `wrapping_*`) to match native i32 two's-complement.
   - `**` int-power overflow: **saturate to MAX_INT**.
5. **Booleans:** comparisons/logicals → int 0/1; truthy = nonzero (incl. negative & nonzero
   float); `&&`/`||` **short-circuit** via conditional jumps; `^^` does not. (§5)
6. **Ranges:** implement all four endpoint variants; `!=` negates membership; tri-operand float
   promotion; bottom-propagating; reject intervals after any operator except `=`/`!=`. (§6)
7. **Chaining:** allow left-associative relational chains (`a<b<c` ⇒ `(a<b)<c`); optional lint
   warning; **not** Python-style. (§7)
8. **Builtins:** `abs` type-preserving; `floor`/`ceil` → int (int arg passes through); `min`/`max`
   **type-preserving** (intentional divergence from Ikemen's float-always); trig/exp/ln/log → f32;
   `random` → int inclusive range via the LCG. (§8)
9. **Precedence:** implement the 13-level table in §9 exactly; `**` right-assoc & above `*`;
   bitwise below equality (C-like); unary right-assoc.
10. **`:=`:** var-LHS only; returns the stored value (int for var/sysvar, f32 for fvar/sysfvar);
    RHS-first eval; right-associative. (§10)
11. **Determinism:** Park–Miller LCG (16807/127773/2836) with `randseed` in rollback state; f32
    floats rounded after each op; pin transcendental implementations. (§11)

### The CB4 recommendation, one line

> **Make the lexer (and the evaluator's float→int narrowing) SATURATE to `i32::MIN`/`i32::MAX`
> with a `tracing::warn!`, replacing the current `overflow → 0`** — this is exactly what Ikemen
> GO does (`Atoi`/`number()` clamp to `IMax`/`IErr`, and Go's `float64→int32` saturates), and it
> is what the community content corpus has been validated against. Keep **wrapping** only for the
> native `i32` `+ - *` arithmetic path (`wrapping_add/sub/mul`).

---

## Sources

**Elecbyte official documentation (the documented contract):**
- CNS format / Expressions section — <https://www.elecbyte.com/mugendocs/cns.html>
- Trigger reference (function list, math functions, cond/ifelse) — <https://www.elecbyte.com/mugendocs/trigger.html>
- BGS / stage doc (version-gated int-vs-fractional camera note) — <https://www.elecbyte.com/mugendocs/bgs.html>
- Community mirror of the CNS expression rules (corroboration of exact wording on bottom, `/`,
  `%`, `**` overflow→MAX_INT, intervals, "precedence same as C") —
  <https://mugen-net.work/wiki/index.php/M.U.G.E.N_Documentation:CNS_Format>

**Ikemen GO — MIT-licensed reference implementation (de-facto behavior).** Repo:
<https://github.com/ikemen-engine/Ikemen-GO>. Files and the functions consulted (line numbers
from a 2026-06-13 `--depth 1` clone; treat function names as the stable anchor):
- `src/bytecode.go` — `BytecodeValue`/`ValueType` (~75, ~1080); `ToI`/`ToF`/`ToB` (~1093–1120);
  arithmetic `neg/not/pow/mul/div/mod/add/sub` (~1340–1474); comparisons `gt/ge/lt/le/eq/ne`
  (~1476–1593); `rangeCheck` (~1525); logical `bland/blor/blxor/blnot` (~1351–1662); bitwise
  `and/or/xor` (~1595–1628); math `abs/exp/ln/log/cos/sin/tan/acos/asin/atan/floor/ceil/min/max/
  random/round/clamp` (~1664–1827); store opcodes `OC_st_var`/`OC_st_fvar` (~2389).
- `src/compiler.go` — recursive-descent precedence chain `expBoolOr/expBoolXor/expBoolAnd/expOr/
  expXor/expAnd/expEqne/expGrls/expAdsb/expMldv/expPow/expPostNot/expValue` (~5312–5748);
  short-circuit jumps `OC_jz`/`OC_jnz` (~5688/5733); `number()` literal saturation (~696);
  range compile `expRange` (~5478); `:=` assignment in `_var` (~1289); `pi`/`e` constants (~4088).
- `src/common.go` — `Atoi` literal saturation to `IMax`/`IErr` (~171); `IMax`/`IErr` (~23);
  `Random` Park–Miller LCG + `RandI`/`Rand`/`Srand` (~27–59); `Min`/`Max`/`Abs`/`Pow` helpers
  (~65–127).
- `src/char.go` — `varSet`/`fvarSet`/`cnsVarSet` return-the-stored-value behavior (~7903–7973).

**Empirical verification** (Go `float64→int32` and `strconv` overflow run locally, 2026-06-13):
`int32(2147483648.0)==2147483647`, `int32(-2147483649.0)==-2147483648`, `int32(+Inf)==2147483647`,
`int32(3.9)==3`, `int32(-3.9)==-3` — confirming Go (and thus Ikemen) **saturates** float→int and
**truncates toward zero**.

_Compiled 2026-06-13 for fp-vm task 4.4 / backlog CB4._
