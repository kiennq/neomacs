//! Built-in primitive functions.
//!
//! All functions here take pre-evaluated `Vec<Value>` arguments and return `EvalResult`.
//! The evaluator dispatches here after evaluating the argument expressions.

pub(crate) use crate::emacs_core::error::{
    expect_args, expect_args_range, expect_fixnum, expect_max_args, expect_min_args,
};
use malachite::base::num::conversion::traits::RoundingFrom;
use malachite::base::rounding_modes::RoundingMode;
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};

/// Debug flag: when true, log every dispatch_builtin call name.
/// Activated after window-setup-hook completes during startup.
static TRACE_ALL_BUILTINS: AtomicBool = AtomicBool::new(false);

pub(crate) use super::buffer::lisp_string_from_buffer_bytes;
pub(super) use super::error::{EvalResult, Flow, LispCondition, signal};
pub(super) use super::intern::{SymId, intern, resolve_sym};
pub(super) use super::keyboard::pure::{
    KEY_CHAR_CODE_MASK, KEY_CHAR_META, convert_lucid_event_list, describe_single_key_value,
    key_sequence_values,
};
pub(super) use super::value::*;
pub(super) use std::cell::RefCell;
pub(super) use std::collections::{HashMap, HashSet};
pub(crate) use strings::downcase_char_code_emacs_compat;
pub(crate) use strings::upcase_char_code_emacs_compat;

// ---------------------------------------------------------------------------
// Transitional string character iteration
// ---------------------------------------------------------------------------

/// Iterate Emacs character codes from a `LispString`.
///
/// For **multibyte** strings each character is decoded straight from the
/// Emacs-internal bytes via `string_char_unchecked`: standard UTF-8 code points
/// (including real Private-Use-Area glyphs such as nerd-font icons) and the
/// extended `0x3FFF00+byte` sequences for eight-bit raw bytes. There is no
/// in-Unicode "sentinel" remapping — that conflated real PUA characters with
/// raw bytes and corrupted them (issue #131). For **unibyte** strings each byte
/// maps to its value directly (0..255).
pub(crate) fn lisp_string_char_codes(string: &crate::heap_types::LispString) -> Vec<u32> {
    let bytes = string.as_bytes();
    if !string.is_multibyte() {
        return bytes.iter().map(|&b| b as u32).collect();
    }
    let mut out = Vec::with_capacity(string.schars());
    let mut pos = 0;
    while pos < bytes.len() {
        let byte = bytes[pos];
        if byte < 0x80 {
            out.push(byte as u32);
            pos += 1;
            continue;
        }
        let (cp, len) = crate::emacs_core::emacs_char::string_char_unchecked(&bytes[pos..]);
        out.push(cp);
        pos += len;
    }
    out
}

/// Return the character code at character index `idx` in `string`, or
/// `None` if `idx` is out of range. Unlike `lisp_string_char_codes`, this
/// does not allocate a `Vec<u32>` — it walks bytes only as far as needed.
/// Mirrors the byte-level access pattern used by GNU's `Faref` on strings
/// (fns.c:3108-3123).
pub(crate) fn lisp_string_char_at(
    string: &crate::heap_types::LispString,
    idx: usize,
) -> Option<u32> {
    let bytes = string.as_bytes();
    if !string.is_multibyte() {
        return bytes.get(idx).map(|&b| b as u32);
    }
    if idx >= string.schars() {
        return None;
    }
    let byte_pos = crate::emacs_core::emacs_char::char_to_byte_pos(bytes, idx);
    let (cp, _) = crate::emacs_core::emacs_char::string_char_unchecked(&bytes[byte_pos..]);
    Some(cp)
}

/// Iterate character codes via a closure (avoids allocation when possible).
pub(crate) fn for_each_lisp_string_char(
    string: &crate::heap_types::LispString,
    mut f: impl FnMut(u32),
) {
    let bytes = string.as_bytes();
    if !string.is_multibyte() {
        for &b in bytes {
            f(b as u32);
        }
        return;
    }
    let mut pos = 0;
    while pos < bytes.len() {
        let (cp, len) = crate::emacs_core::emacs_char::string_char_unchecked(&bytes[pos..]);
        f(cp);
        pos += len;
    }
}

/// Reset all thread-local state in builtins (called from Context::new).
pub(crate) fn reset_builtins_thread_locals() {
    collections::reset_collections_thread_locals();
    stubs::reset_stubs_thread_locals();
    hooks::reset_hooks_thread_locals();
    symbols::reset_symbols_thread_locals();
}

pub use stubs::{NeomacsMonitorInfo, neomacs_monitor_info_snapshot, set_neomacs_monitor_info};

/// Extract an integer, signaling wrong-type-argument if not.
pub(super) fn expect_int(value: &Value) -> Result<i64, Flow> {
    match value.kind() {
        ValueKind::Fixnum(n) => Ok(n),
        _other => Err(signal(
            LispCondition::WrongTypeArgument,
            vec![Value::symbol("integerp"), *value],
        )),
    }
}

pub(super) fn expect_char_table_index(value: &Value) -> Result<i64, Flow> {
    let idx = expect_fixnum(value)?;
    if !(0..=0x3F_FFFF).contains(&idx) {
        maybe_trace_characterp_nil(value, "expect_char_table_index");
        return Err(signal(
            LispCondition::WrongTypeArgument,
            vec![Value::symbol("characterp"), *value],
        ));
    }
    Ok(idx)
}

pub(super) fn expect_char_equal_code(value: &Value) -> Result<i64, Flow> {
    match value.kind() {
        ValueKind::Fixnum(n) if (0..=KEY_CHAR_CODE_MASK).contains(&n) => Ok(n),
        _other => {
            maybe_trace_characterp_nil(value, "expect_char_equal_code");
            Err(signal(
                LispCondition::WrongTypeArgument,
                vec![Value::symbol("characterp"), *value],
            ))
        }
    }
}

pub(super) fn expect_character_code(value: &Value) -> Result<i64, Flow> {
    match value.kind() {
        ValueKind::Fixnum(c) if (0..=0x3F_FFFF).contains(&c) => Ok(c),
        _other => {
            maybe_trace_characterp_nil(value, "expect_character_code");
            Err(signal(
                LispCondition::WrongTypeArgument,
                vec![Value::symbol("characterp"), *value],
            ))
        }
    }
}

pub(crate) fn character_code_to_rust_char(code: i64) -> Option<char> {
    let code = code as u32;
    char::from_u32(code).or_else(|| {
        crate::emacs_core::emacs_char::char_byte8_p(code).then(|| {
            char::from_u32(crate::emacs_core::emacs_char::char_to_byte8(code) as u32)
                .expect("raw byte values must be valid Unicode scalars")
        })
    })
}

fn maybe_trace_characterp_nil(value: &Value, source: &str) {
    if !value.is_nil() {
        return;
    }
    if std::env::var("NEOVM_TRACE_CHARACTERP_NIL").unwrap_or_default() != "1" {
        return;
    }
    eprintln!(
        "NEOVM_TRACE_CHARACTERP_NIL source={source}\n{}",
        std::backtrace::Backtrace::force_capture()
    );
}

pub(super) fn char_equal_folded(code: i64) -> Option<String> {
    char::from_u32(code as u32).map(|ch| ch.to_lowercase().collect())
}

/// Extract an integer/marker-ish position value.
///
/// GNU Emacs accepts marker designators anywhere `integer-or-marker-p`
/// is allowed, using the marker's current position.
pub(super) fn expect_integer_or_marker(value: &Value) -> Result<i64, Flow> {
    match value.kind() {
        ValueKind::Fixnum(n) => Ok(n),
        _ if super::marker::is_marker(value) => super::marker::marker_position_as_int(value),
        _ => Err(signal(
            LispCondition::WrongTypeArgument,
            vec![Value::symbol("integer-or-marker-p"), *value],
        )),
    }
}

pub(super) fn expect_integer_or_marker_eval(
    eval: &super::eval::Context,
    value: &Value,
) -> Result<i64, Flow> {
    match value.kind() {
        ValueKind::Fixnum(n) => Ok(n),
        _ if super::marker::is_marker(value) => {
            super::marker::marker_position_as_int_eval(eval, value)
        }
        _ => Err(signal(
            LispCondition::WrongTypeArgument,
            vec![Value::symbol("integer-or-marker-p"), *value],
        )),
    }
}

/// Extract a non-negative integer, signaling `wholenump` on failure.
pub(super) fn expect_wholenump(value: &Value) -> Result<i64, Flow> {
    let n = match value.kind() {
        ValueKind::Fixnum(n) => n,
        _ => {
            return Err(signal(
                LispCondition::WrongTypeArgument,
                vec![Value::symbol("wholenump"), *value],
            ));
        }
    };
    if n < 0 {
        return Err(signal(
            LispCondition::WrongTypeArgument,
            vec![Value::symbol("wholenump"), *value],
        ));
    }
    Ok(n)
}

#[derive(Clone, Copy, Debug)]
pub(super) enum NumberOrMarker {
    Int(i64),
    Float(f64),
}

pub(super) fn expect_number_or_marker(value: &Value) -> Result<NumberOrMarker, Flow> {
    match value.kind() {
        ValueKind::Fixnum(n) => Ok(NumberOrMarker::Int(n)),
        ValueKind::Float => Ok(NumberOrMarker::Float(value.xfloat())),
        // Bignums lower into f64 for the comparison/numeric path,
        // matching GNU's XFLOATINT behaviour. Callers that need
        // exact arithmetic dispatch on the Value::kind() directly.
        ValueKind::Veclike(VecLikeType::Bignum) => Ok(NumberOrMarker::Float(
            f64::rounding_from(value.as_bignum().unwrap(), RoundingMode::Nearest).0,
        )),
        _ if super::marker::is_marker(value) => Ok(NumberOrMarker::Int(
            super::marker::marker_position_as_int(value)?,
        )),
        _ => Err(signal(
            LispCondition::WrongTypeArgument,
            vec![Value::symbol("number-or-marker-p"), *value],
        )),
    }
}

pub(super) fn expect_number_or_marker_eval(
    eval: &super::eval::Context,
    value: &Value,
) -> Result<NumberOrMarker, Flow> {
    match value.kind() {
        ValueKind::Fixnum(n) => Ok(NumberOrMarker::Int(n)),
        ValueKind::Float => Ok(NumberOrMarker::Float(value.xfloat())),
        ValueKind::Veclike(VecLikeType::Bignum) => Ok(NumberOrMarker::Float(
            f64::rounding_from(value.as_bignum().unwrap(), RoundingMode::Nearest).0,
        )),
        _ if super::marker::is_marker(value) => Ok(NumberOrMarker::Int(
            super::marker::marker_position_as_int_eval(eval, value)?,
        )),
        _ => Err(signal(
            LispCondition::WrongTypeArgument,
            vec![Value::symbol("number-or-marker-p"), *value],
        )),
    }
}

/// Extract a number as f64.
pub(super) fn expect_number(value: &Value) -> Result<f64, Flow> {
    match value.kind() {
        ValueKind::Fixnum(n) => Ok(n as f64),
        ValueKind::Float => Ok(value.xfloat()),
        ValueKind::Veclike(VecLikeType::Bignum) => {
            Ok(f64::rounding_from(value.as_bignum().unwrap(), RoundingMode::Nearest).0)
        }
        _ => Err(signal(
            LispCondition::WrongTypeArgument,
            vec![Value::symbol("numberp"), *value],
        )),
    }
}

pub(super) fn expect_number_or_marker_f64(value: &Value) -> Result<f64, Flow> {
    match expect_number_or_marker(value)? {
        NumberOrMarker::Int(n) => Ok(n as f64),
        NumberOrMarker::Float(f) => Ok(f),
    }
}

pub(super) fn expect_number_or_marker_f64_eval(
    eval: &super::eval::Context,
    value: &Value,
) -> Result<f64, Flow> {
    match expect_number_or_marker_eval(eval, value)? {
        NumberOrMarker::Int(n) => Ok(n as f64),
        NumberOrMarker::Float(f) => Ok(f),
    }
}

pub(super) fn expect_integer_or_marker_after_number_check(value: &Value) -> Result<i64, Flow> {
    match expect_number_or_marker(value)? {
        NumberOrMarker::Int(n) => Ok(n),
        NumberOrMarker::Float(_) => Err(signal(
            LispCondition::WrongTypeArgument,
            vec![Value::symbol("integer-or-marker-p"), *value],
        )),
    }
}

pub(super) fn expect_integer_or_marker_after_number_check_eval(
    eval: &super::eval::Context,
    value: &Value,
) -> Result<i64, Flow> {
    match expect_number_or_marker_eval(eval, value)? {
        NumberOrMarker::Int(n) => Ok(n),
        NumberOrMarker::Float(_) => Err(signal(
            LispCondition::WrongTypeArgument,
            vec![Value::symbol("integer-or-marker-p"), *value],
        )),
    }
}

/// True if any arg is a float (triggers float arithmetic).
pub(super) fn has_float(args: &[Value]) -> bool {
    args.iter().any(|v| v.is_float())
}

pub(super) fn normalize_string_start_arg(
    string: &str,
    start: Option<&Value>,
) -> Result<usize, Flow> {
    let Some(start_val) = start else {
        return Ok(0);
    };
    if start_val.is_nil() {
        return Ok(0);
    }

    let raw_start = expect_fixnum(start_val)?;
    let len = string.chars().count() as i64;
    let normalized = if raw_start < 0 {
        len.checked_add(raw_start)
    } else {
        Some(raw_start)
    };

    let Some(start_idx) = normalized else {
        return Err(signal(
            LispCondition::ArgsOutOfRange,
            vec![Value::string(string), Value::fixnum(raw_start)],
        ));
    };

    if !(0..=len).contains(&start_idx) {
        return Err(signal(
            LispCondition::ArgsOutOfRange,
            vec![Value::string(string), Value::fixnum(raw_start)],
        ));
    }

    let start_char_idx = start_idx as usize;
    if start_char_idx == len as usize {
        return Ok(string.len());
    }

    Ok(string
        .char_indices()
        .nth(start_char_idx)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(string.len()))
}

// Re-export sibling modules so submodules can use `super::eval`, `super::marker`, etc.
pub(super) use super::autoload;
pub(super) use super::builtins_extra;
pub(super) use super::ccl;
pub(super) use super::charset;
pub(super) use super::chartable;
pub(super) use super::editfns;
pub(super) use super::error;
pub(super) use super::eval;
pub(super) use super::fileio;
pub(super) use super::kbd;
pub(super) use super::keymap;
pub(super) use super::load;
pub(super) use super::marker;
pub(super) use super::navigation;
pub(super) use super::print;
pub(super) use super::regex;
pub(super) use super::subr_info;
pub(super) use super::terminal;
pub(super) use super::textprop;
pub(super) use super::value;
pub(super) use super::window_cmds;

// --- Submodules ---
mod arithmetic;
mod buffer_text_backend;
pub(crate) mod collections;
mod cons_list;
mod effects;
pub(crate) mod from_value;
pub(crate) mod misc_pure;
pub(crate) mod strings;
pub(crate) mod types;

pub(crate) use arithmetic::*;
pub(crate) use buffer_text_backend::*;
pub(crate) use collections::*;
pub use cons_list::lambda_params_to_value;
pub use cons_list::lambda_to_closure_vector;
pub use cons_list::parse_lambda_params_from_value;
pub(crate) use cons_list::*;
pub(crate) use from_value::*;
pub(crate) use misc_pure::*;
pub(crate) use strings::*;
pub(crate) use types::*;

// `pub(crate)` so the R2 JIT Tier-A CallBuiltinSym read shim
// (`jit::compile::neovm_jit_cbsym_read`) can DELEGATE to the GC-free buffer
// primitive bodies (`builtin_point_0`, `builtin_char_after`, ...) by name
// instead of reimplementing them (matches the sibling `navigation`/`editfns`/
// `search` modules, already crate-visible).
mod file_notify;
pub(crate) mod fringe_bitmap;
pub(crate) mod fringe_standard_bitmaps;
pub(crate) mod gnutls;
pub(crate) mod higher_order;
mod hooks;
pub(crate) mod keymaps;
mod lcms;
pub(crate) mod misc_eval;
pub(crate) mod search;
mod stubs;
pub(crate) mod symbols;
mod treesit;

pub(crate) use super::buffer::*;
pub(crate) use file_notify::*;
pub(crate) use higher_order::*;
pub(crate) use hooks::*;
pub(crate) use keymaps::*;
pub(crate) use misc_eval::*;
pub(crate) use search::*;
pub(crate) use stubs::*;
pub(crate) use symbols::*;
pub(crate) use treesit::*;

// ===========================================================================
// Helpers
// ===========================================================================

/// Borrow a string argument's payload, tied to the `Value` place it came from.
///
/// The returned lifetime is elided to `value`'s rather than `'static`
/// (DIVERGENCES.md 163). Almost every caller passes `&args[i]`, so the borrow
/// is tied to the argument slice — which is exactly what keeps the string
/// alive: `apply_internal`'s backtrace frame roots the arguments for the whole
/// subr call, the way GNU's `mark_specpdl` marks `backtrace_args`. Saying
/// `'static` claimed something stronger and stopped the compiler from
/// noticing when a borrow outlived the argument list.
pub(super) fn expect_lisp_string(value: &Value) -> Result<&crate::heap_types::LispString, Flow> {
    value.as_lisp_string().ok_or_else(|| {
        signal(
            LispCondition::WrongTypeArgument,
            vec![Value::symbol("stringp"), *value],
        )
    })
}

/// Validate a string argument and decode it to a Rust `String` for text-only
/// processing (display strings, names, identifiers). Valid Unicode (including
/// real Private-Use glyphs) is preserved exactly; raw eight-bit bytes become
/// U+FFFD. Callers that must preserve raw bytes use `expect_lisp_string`.
pub(super) fn expect_string_lossy(value: &Value) -> Result<String, Flow> {
    expect_lisp_string(value).map(|ls| crate::emacs_core::emacs_char::to_utf8_lossy(ls.as_bytes()))
}

/// GNU's exact comparison operand: a string passes through unchanged, while a
/// symbol contributes its existing `SYMBOL_NAME` string object.
///
/// Returns the OPERAND, not a borrow of it. GNU's own primitives are written
/// the same way -- `if (SYMBOLP (s1)) s1 = SYMBOL_NAME (s1);` and only then
/// `SDATA (s1)` (`src/fns.c:344-353`) -- and it is what lets this function
/// stop claiming `'static` for a heap string: see [`StringDesignator`].
pub(super) fn expect_string_comparison_operand(
    value: &Value,
) -> Result<from_value::StringDesignator, Flow> {
    match value.kind() {
        ValueKind::String => Ok(from_value::StringDesignator::String(*value)),
        _ => value
            .as_symbol_id()
            .map(crate::emacs_core::intern::resolve_lisp_visible_symbol_name)
            .map(from_value::StringDesignator::SymbolName)
            .ok_or_else(|| {
                signal(
                    LispCondition::WrongTypeArgument,
                    vec![Value::symbol("stringp"), *value],
                )
            }),
    }
}

/// Build a `LispString` from a plain (sentinel-free) Rust `&str`, preserving the
/// caller's multibyteness choice.
///
/// Every Lisp-visible string built from a Rust `&str` (doc strings, parsed file
/// data, filenames, pdump payloads, printer output) goes through here: the str
/// carries no storage-String sentinels, so its bytes are already in Emacs
/// internal form and become the `LispString` directly. The legacy
/// storage-decode round-trip that this replaced has been retired (issue #131);
/// the storage codec now survives only inside the buffer-text/runtime-reader
/// layer (`storage_string_to_buffer_bytes`), which is unrelated to the
/// Lisp-string path.
pub(crate) fn plain_str_to_lisp_string(
    text: &str,
    multibyte: bool,
) -> crate::heap_types::LispString {
    if multibyte {
        crate::heap_types::LispString::from_emacs_bytes(text.as_bytes().to_vec())
    } else {
        crate::heap_types::LispString::from_unibyte(text.as_bytes().to_vec())
    }
}

/// Test-only convenience: decode a string Value to a lossy `String` (valid
/// Unicode preserved, raw eight-bit -> U+FFFD). No longer produces a storage
/// string; production code uses `as_lisp_string` for byte-faithful access.
/// `#[cfg(test)]`-gated so this lossy helper can never re-enter a production
/// path (issue #131).
#[cfg(test)]
pub(crate) fn lisp_string_to_runtime_string(value: Value) -> String {
    value
        .as_lisp_string()
        .map(|ls| crate::emacs_core::emacs_char::to_utf8_lossy(ls.as_bytes()))
        .expect("ValueKind::String must carry LispString payload")
}

// Search / regex builtins are defined at the end of this file.

/// Try to dispatch a builtin function by name. Returns None if not a known builtin.
pub(crate) fn dispatch_builtin(
    eval: &mut super::eval::Context,
    name: &str,
    args: Vec<Value>,
) -> Option<EvalResult> {
    dispatch_builtin_by_id(eval, intern(name), args)
}

/// Try to dispatch a builtin function by its canonical symbol id.
pub(crate) fn dispatch_builtin_by_id(
    eval: &mut super::eval::Context,
    sym_id: SymId,
    args: Vec<Value>,
) -> Option<EvalResult> {
    eval.dispatch_subr_value(Value::subr_from_sym_id(sym_id), args)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuiltinNoEvalPlaceholder {
    Nil,
    FixnumZero,
    WindowLineHeight,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuiltinNoEvalPolicy {
    Native,
    RequiresEvalState,
    Placeholder(BuiltinNoEvalPlaceholder),
}

static BUILTIN_NO_EVAL_POLICIES: OnceLock<Mutex<Vec<Option<BuiltinNoEvalPolicy>>>> =
    OnceLock::new();

fn builtin_no_eval_policies() -> &'static Mutex<Vec<Option<BuiltinNoEvalPolicy>>> {
    BUILTIN_NO_EVAL_POLICIES.get_or_init(|| Mutex::new(Vec::new()))
}

fn record_builtin_no_eval_policy(name: &str, policy: BuiltinNoEvalPolicy) {
    let sym_id = intern(name);
    let mut policies = builtin_no_eval_policies()
        .lock()
        .expect("builtin no-eval policy registry poisoned");
    let index = sym_id.0 as usize;
    if policies.len() <= index {
        policies.resize(index + 1, None);
    }
    policies[index] = Some(policy);
}

#[allow(dead_code)] // grandfathered when dead_code lint was enabled; delete or wire up
fn builtin_no_eval_policy(sym_id: SymId) -> BuiltinNoEvalPolicy {
    builtin_no_eval_policies()
        .lock()
        .expect("builtin no-eval policy registry poisoned")
        .get(sym_id.0 as usize)
        .copied()
        .flatten()
        .unwrap_or(BuiltinNoEvalPolicy::Native)
}

#[allow(dead_code)] // grandfathered when dead_code lint was enabled; delete or wire up
fn dispatch_builtin_stateless_placeholder(
    policy: BuiltinNoEvalPolicy,
    args: &[Value],
) -> Option<EvalResult> {
    let value = match policy {
        BuiltinNoEvalPolicy::Placeholder(BuiltinNoEvalPlaceholder::Nil) => Value::NIL,
        BuiltinNoEvalPolicy::Placeholder(BuiltinNoEvalPlaceholder::FixnumZero) => Value::fixnum(0),
        BuiltinNoEvalPolicy::Placeholder(BuiltinNoEvalPlaceholder::WindowLineHeight) => {
            if args.len() == 2 && args[1].as_symbol_name() == Some("window") {
                Value::NIL
            } else {
                return None;
            }
        }
        BuiltinNoEvalPolicy::Native | BuiltinNoEvalPolicy::RequiresEvalState => return None,
    };
    Some(Ok(value))
}

#[cfg(test)]
pub(crate) fn dispatch_builtin_without_eval_state(
    name: &str,
    args: Vec<Value>,
) -> Option<EvalResult> {
    use crate::emacs_core::eval::Context;

    thread_local! {
        static CTX: std::cell::RefCell<Context> = std::cell::RefCell::new(Context::new());
    }

    CTX.with(|cell| {
        let ctx = &mut *cell.borrow_mut();
        let sym_id = intern(name);
        let policy = builtin_no_eval_policy(sym_id);
        if let Some(result) = dispatch_builtin_stateless_placeholder(policy, &args) {
            return Some(result);
        }
        if policy == BuiltinNoEvalPolicy::RequiresEvalState {
            return None;
        }
        dispatch_builtin_by_id(ctx, sym_id, args)
    })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod replace_region_contents_test;

#[cfg(test)]
mod lisp_only_predicates_and_aliases_test;

#[cfg(test)]
mod lisp_only_undo_commands_test;

#[cfg(test)]
mod process_launchers_are_lisp_only_test;

#[cfg(test)]
mod lisp_only_misc_names_test;

#[cfg(test)]
mod lisp_only_window_frame_names_test;

#[cfg(test)]
mod rust_subrs_shadowed_by_lisp_test;

// -----------------------------------------------------------------------
// Wrapper functions for builtins that need tracing or non-standard access
// -----------------------------------------------------------------------

fn defsubr_run_hooks(eval: &mut super::eval::Context, args: Vec<Value>) -> EvalResult {
    let hook_names: Vec<String> = args
        .iter()
        .filter_map(|a| a.as_symbol_name().map(|s| s.to_string()))
        .collect();
    let dominated_by_noise = hook_names
        .iter()
        .all(|h| h == "custom-define-hook" || h == "change-major-mode-hook");
    tracing::debug!(hooks = ?hook_names, noisy = dominated_by_noise, "run-hooks called");
    let result = builtin_run_hooks(eval, args);
    tracing::debug!(hooks = ?hook_names, noisy = dominated_by_noise, "run-hooks returned");
    if hook_names.iter().any(|h| h == "window-setup-hook") {
        tracing::debug!("Enabling post-startup builtin tracing");
        TRACE_ALL_BUILTINS.store(true, Ordering::Relaxed);
    }
    result
}

fn defsubr_load(eval: &mut super::eval::Context, args: Vec<Value>) -> EvalResult {
    let file_name = args.first().map(|a| format!("{}", a)).unwrap_or_default();
    tracing::debug!(file = %file_name, "load called");
    let result = builtin_load(eval, args);
    tracing::debug!(file = %file_name, ok = result.is_ok(), "load returned");
    result
}

fn defsubr_message(eval: &mut super::eval::Context, args: Vec<Value>) -> EvalResult {
    let msg_preview: String = args
        .first()
        .map(|a| {
            let s = format!("{}", a);
            if s.len() > 120 {
                format!("{}...", &s[..120])
            } else {
                s
            }
        })
        .unwrap_or_default();
    tracing::debug!(msg = %msg_preview, "message");
    builtin_message(eval, args)
}

fn defsubr_coding_system_aliases(eval: &mut super::eval::Context, args: Vec<Value>) -> EvalResult {
    super::coding::builtin_coding_system_aliases(&eval.coding_systems, args)
}
fn defsubr_coding_system_plist(eval: &mut super::eval::Context, args: Vec<Value>) -> EvalResult {
    super::coding::builtin_coding_system_plist(&eval.coding_systems, args)
}
fn defsubr_coding_system_put(eval: &mut super::eval::Context, args: Vec<Value>) -> EvalResult {
    super::coding::builtin_coding_system_put(&mut eval.coding_systems, args)
}
fn defsubr_coding_system_base(eval: &mut super::eval::Context, args: Vec<Value>) -> EvalResult {
    super::coding::builtin_coding_system_base(&eval.coding_systems, args)
}
fn defsubr_coding_system_eol_type(eval: &mut super::eval::Context, args: Vec<Value>) -> EvalResult {
    super::coding::builtin_coding_system_eol_type(&eval.coding_systems, args)
}
fn defsubr_detect_coding_string(eval: &mut super::eval::Context, args: Vec<Value>) -> EvalResult {
    super::coding::builtin_detect_coding_string(&eval.coding_systems, args)
}
fn defsubr_detect_coding_region(eval: &mut super::eval::Context, args: Vec<Value>) -> EvalResult {
    super::coding::builtin_detect_coding_region(&eval.coding_systems, &eval.buffers, args)
}
fn defsubr_keyboard_coding_system(eval: &mut super::eval::Context, args: Vec<Value>) -> EvalResult {
    super::coding::builtin_keyboard_coding_system(&eval.coding_systems, args)
}
fn defsubr_terminal_coding_system(eval: &mut super::eval::Context, args: Vec<Value>) -> EvalResult {
    super::coding::builtin_terminal_coding_system(&eval.coding_systems, args)
}
fn defsubr_coding_system_priority_list(
    eval: &mut super::eval::Context,
    args: Vec<Value>,
) -> EvalResult {
    super::coding::builtin_coding_system_priority_list(&eval.coding_systems, args)
}

fn defsubr_coding_system_p(eval: &mut super::eval::Context, args: Vec<Value>) -> EvalResult {
    super::coding::builtin_coding_system_p(&eval.coding_systems, args)
}
fn defsubr_check_coding_system(eval: &mut super::eval::Context, args: Vec<Value>) -> EvalResult {
    super::coding::builtin_check_coding_system(&eval.coding_systems, args)
}
fn defsubr_check_coding_systems_region(
    eval: &mut super::eval::Context,
    args: Vec<Value>,
) -> EvalResult {
    super::coding::builtin_check_coding_systems_region(eval, args)
}
fn defsubr_define_coding_system_internal(
    eval: &mut super::eval::Context,
    args: Vec<Value>,
) -> EvalResult {
    let result = super::coding::builtin_define_coding_system_internal(
        &mut eval.coding_systems,
        args.clone(),
    )?;
    super::coding::record_lisp_define_coding_system_internal(&mut eval.obarray, &args);
    Ok(result)
}
fn defsubr_define_coding_system_alias(
    eval: &mut super::eval::Context,
    args: Vec<Value>,
) -> EvalResult {
    let result =
        super::coding::builtin_define_coding_system_alias(&mut eval.coding_systems, args.clone())?;
    super::coding::record_lisp_define_coding_system_alias(&mut eval.obarray, &args);
    Ok(result)
}
fn defsubr_set_coding_system_priority(
    eval: &mut super::eval::Context,
    args: Vec<Value>,
) -> EvalResult {
    let result = super::coding::builtin_set_coding_system_priority(&mut eval.coding_systems, args)?;
    // GNU `Fset_coding_system_priority` also rebuilds the `coding-category-list`
    // variable (coding.c) from the reordered category priorities.
    let categories = super::coding::coding_category_priority_list(&eval.coding_systems);
    let list = Value::list(categories.into_iter().map(Value::symbol).collect());
    eval.set_variable("coding-category-list", list);
    Ok(result)
}
fn defsubr_set_keyboard_coding_system_internal(
    eval: &mut super::eval::Context,
    args: Vec<Value>,
) -> EvalResult {
    super::coding::builtin_set_keyboard_coding_system_internal(&mut eval.coding_systems, args)
}
fn defsubr_set_safe_terminal_coding_system_internal(
    eval: &mut super::eval::Context,
    args: Vec<Value>,
) -> EvalResult {
    super::coding::builtin_set_safe_terminal_coding_system_internal(&mut eval.coding_systems, args)
}
fn defsubr_set_terminal_coding_system_internal(
    eval: &mut super::eval::Context,
    args: Vec<Value>,
) -> EvalResult {
    super::coding::builtin_set_terminal_coding_system_internal(&mut eval.coding_systems, args)
}

type BuiltinFn = fn(&mut super::eval::Context, Vec<Value>) -> EvalResult;

/// Initial command-enablement policy installed with a builtin's function cell.
///
/// GNU C code can attach the `disabled` property in the same `syms_of_*`
/// function that registers a subr.  Model that relationship in the builtin
/// descriptor so the function and its startup policy cannot drift into
/// unrelated initialization paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuiltinCommandDefault {
    Enabled,
    Disabled,
}

#[derive(Clone, Copy)]
struct BuiltinRegistration {
    name: &'static str,
    func: BuiltinFn,
    min_args: u16,
    max_args: Option<u16>,
    interactive_spec: Option<super::interactive::BuiltinInteractiveSpec>,
    no_eval_policy: BuiltinNoEvalPolicy,
    command_default: BuiltinCommandDefault,
}

impl BuiltinRegistration {
    const fn requires_eval_state(
        name: &'static str,
        func: BuiltinFn,
        min_args: u16,
        max_args: Option<u16>,
    ) -> Self {
        Self {
            name,
            func,
            min_args,
            max_args,
            interactive_spec: None,
            no_eval_policy: BuiltinNoEvalPolicy::RequiresEvalState,
            command_default: BuiltinCommandDefault::Enabled,
        }
    }

    const fn placeholder(
        name: &'static str,
        func: BuiltinFn,
        min_args: u16,
        max_args: Option<u16>,
        placeholder: BuiltinNoEvalPlaceholder,
    ) -> Self {
        Self {
            name,
            func,
            min_args,
            max_args,
            interactive_spec: None,
            no_eval_policy: BuiltinNoEvalPolicy::Placeholder(placeholder),
            command_default: BuiltinCommandDefault::Enabled,
        }
    }

    const fn disabled_command(
        name: &'static str,
        func: BuiltinFn,
        min_args: u16,
        max_args: Option<u16>,
    ) -> Self {
        Self {
            name,
            func,
            min_args,
            max_args,
            interactive_spec: None,
            no_eval_policy: BuiltinNoEvalPolicy::Native,
            command_default: BuiltinCommandDefault::Disabled,
        }
    }

    const fn interactive(
        mut self,
        interactive_spec: super::interactive::BuiltinInteractiveSpec,
    ) -> Self {
        self.interactive_spec = Some(interactive_spec);
        self
    }
}

fn region_noncontiguous_interactive_spec() -> Value {
    Value::list(vec![
        Value::symbol("list"),
        Value::list(vec![Value::symbol("region-beginning")]),
        Value::list(vec![Value::symbol("region-end")]),
        Value::list(vec![Value::symbol("region-noncontiguous-p")]),
    ])
}

fn goto_char_interactive_spec() -> Value {
    Value::list(vec![
        Value::symbol("goto-char--read-natnum-interactive"),
        Value::string("Go to char: "),
    ])
}

fn insert_char_interactive_spec() -> Value {
    Value::list(vec![
        Value::symbol("list"),
        Value::list(vec![
            Value::symbol("read-char-by-name"),
            Value::string("Insert character (Unicode name or hex): "),
        ]),
        Value::list(vec![
            Value::symbol("prefix-numeric-value"),
            Value::symbol("current-prefix-arg"),
        ]),
        Value::T,
    ])
}

fn rename_buffer_interactive_spec() -> Value {
    Value::list(vec![
        Value::symbol("list"),
        Value::list(vec![
            Value::symbol("read-string"),
            Value::string("Rename buffer (to new name): "),
            Value::NIL,
            Value::list(vec![
                Value::symbol("quote"),
                Value::symbol("buffer-name-history"),
            ]),
            Value::list(vec![
                Value::symbol("buffer-name"),
                Value::list(vec![Value::symbol("current-buffer")]),
            ]),
        ]),
        Value::symbol("current-prefix-arg"),
    ])
}

fn self_insert_command_interactive_spec() -> Value {
    Value::list(vec![
        Value::symbol("list"),
        Value::list(vec![
            Value::symbol("prefix-numeric-value"),
            Value::symbol("current-prefix-arg"),
        ]),
        Value::symbol("last-command-event"),
    ])
}

fn delete_process_interactive_spec() -> Value {
    Value::list(vec![
        Value::symbol("list"),
        Value::list(vec![Value::symbol("quote"), Value::symbol("message")]),
    ])
}

fn kill_process_interactive_spec() -> Value {
    Value::list(vec![
        Value::symbol("list"),
        Value::list(vec![
            Value::symbol("read-process-name"),
            Value::string("Kill process"),
        ]),
    ])
}

fn signal_process_interactive_spec() -> Value {
    Value::list(vec![
        Value::symbol("list"),
        Value::list(vec![
            Value::symbol("read-string"),
            Value::string("Process (name or number): "),
        ]),
        Value::list(vec![Value::symbol("read-signal-name")]),
    ])
}

fn set_file_modes_interactive_spec() -> Value {
    Value::list(vec![
        Value::symbol("let"),
        Value::list(vec![Value::list(vec![
            Value::symbol("file"),
            Value::list(vec![
                Value::symbol("read-file-name"),
                Value::string("File: "),
            ]),
        ])]),
        Value::list(vec![
            Value::symbol("list"),
            Value::symbol("file"),
            Value::list(vec![
                Value::symbol("read-file-modes"),
                Value::NIL,
                Value::symbol("file"),
            ]),
        ]),
    ])
}

fn set_frame_property_interactive_spec(prompt: &'static str, getter: &'static str) -> Value {
    Value::list(vec![
        Value::symbol("set-frame-property--interactive"),
        Value::string(prompt),
        Value::list(vec![Value::symbol(getter)]),
    ])
}

fn set_frame_height_interactive_spec() -> Value {
    set_frame_property_interactive_spec("Frame height: ", "frame-height")
}

fn set_frame_width_interactive_spec() -> Value {
    set_frame_property_interactive_spec("Frame width: ", "frame-width")
}

fn lossage_size_interactive_spec() -> Value {
    Value::list(vec![
        Value::symbol("list"),
        Value::list(vec![
            Value::symbol("read-number"),
            Value::string("Set maximum keystrokes to: "),
            Value::list(vec![Value::symbol("lossage-size")]),
        ]),
    ])
}

fn transpose_regions_interactive_spec() -> Value {
    Value::list(vec![
        Value::symbol("if"),
        Value::list(vec![
            Value::symbol("<"),
            Value::list(vec![Value::symbol("length"), Value::symbol("mark-ring")]),
            Value::fixnum(2),
        ]),
        Value::list(vec![
            Value::symbol("error"),
            Value::string("Other region must be marked before transposing two regions"),
        ]),
        Value::list(vec![
            Value::symbol("let*"),
            Value::list(vec![
                Value::list(vec![
                    Value::symbol("num"),
                    Value::list(vec![
                        Value::symbol("if"),
                        Value::symbol("current-prefix-arg"),
                        Value::list(vec![
                            Value::symbol("prefix-numeric-value"),
                            Value::symbol("current-prefix-arg"),
                        ]),
                        Value::fixnum(0),
                    ]),
                ]),
                Value::list(vec![
                    Value::symbol("ring-length"),
                    Value::list(vec![Value::symbol("length"), Value::symbol("mark-ring")]),
                ]),
                Value::list(vec![
                    Value::symbol("eltnum"),
                    Value::list(vec![
                        Value::symbol("mod"),
                        Value::symbol("num"),
                        Value::symbol("ring-length"),
                    ]),
                ]),
                Value::list(vec![
                    Value::symbol("eltnum2"),
                    Value::list(vec![
                        Value::symbol("mod"),
                        Value::list(vec![Value::symbol("1+"), Value::symbol("num")]),
                        Value::symbol("ring-length"),
                    ]),
                ]),
            ]),
            Value::list(vec![
                Value::symbol("list"),
                Value::list(vec![Value::symbol("point")]),
                Value::list(vec![Value::symbol("mark")]),
                Value::list(vec![
                    Value::symbol("elt"),
                    Value::symbol("mark-ring"),
                    Value::symbol("eltnum"),
                ]),
                Value::list(vec![
                    Value::symbol("elt"),
                    Value::symbol("mark-ring"),
                    Value::symbol("eltnum2"),
                ]),
            ]),
        ]),
    ])
}

/// Diagnostics-only (feature `vm-profile`): clear the VM profiler histograms
/// (OP-MIX + SUBR-MIX + the Op::Call/CallBuiltinSym entry split). Call before a
/// measured batch editing session so loadup/startup traffic is excluded.
#[cfg(feature = "vm-profile")]
fn defsubr_vm_profile_reset(_eval: &mut super::eval::Context, _args: Vec<Value>) -> EvalResult {
    crate::emacs_core::bytecode::vm::vm_profile::reset();
    Ok(Value::NIL)
}

/// Diagnostics-only (feature `vm-profile`): dump the VM profiler histograms to
/// stderr with an optional LABEL (string). Returns nil. Pairs with
/// `neovm--vm-profile-reset` for a reset → workload → dump batch session.
#[cfg(feature = "vm-profile")]
fn defsubr_vm_profile_dump(_eval: &mut super::eval::Context, args: Vec<Value>) -> EvalResult {
    let label = args
        .first()
        .map(|v| format!("{v}").trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "batch".to_string());
    crate::emacs_core::bytecode::vm::vm_profile::dump(&label);
    Ok(Value::NIL)
}

/// Internal test hook: panic with the optional MESSAGE argument. Exists so
/// panic-containment tests (the module ABI today, JIT shims next) can
/// originate a HOST-code panic from Lisp: a foreign Rust module's own panic
/// cannot cross its statically linked std into our `catch_unwind`, and no
/// legitimate Lisp input panics the evaluator on demand.
fn defsubr_neovm_internal_panic(_eval: &mut super::eval::Context, args: Vec<Value>) -> EvalResult {
    let message = args
        .first()
        .and_then(|v| v.as_lisp_string())
        .map(|ls| String::from_utf8_lossy(ls.as_bytes()).into_owned())
        .unwrap_or_else(|| "neovm--internal-panic".to_string());
    panic!("{message}");
}

fn register_builtin(ctx: &mut super::eval::Context, builtin: BuiltinRegistration) {
    if builtin.no_eval_policy != BuiltinNoEvalPolicy::Native {
        record_builtin_no_eval_policy(builtin.name, builtin.no_eval_policy);
    }
    if let Some(interactive_spec) = builtin.interactive_spec {
        ctx.defsubr_interactive(
            builtin.name,
            builtin.func,
            builtin.min_args,
            builtin.max_args,
            interactive_spec,
        );
    } else {
        ctx.defsubr(
            builtin.name,
            builtin.func,
            builtin.min_args,
            builtin.max_args,
        );
    }
    match builtin.command_default {
        BuiltinCommandDefault::Enabled => {}
        BuiltinCommandDefault::Disabled => ctx
            .obarray_mut()
            .put_property(builtin.name, "disabled", Value::T)
            .expect("freshly registered builtin must have a valid property list"),
    }
}

pub(crate) fn register_builtin_requires_eval_state(
    ctx: &mut super::eval::Context,
    name: &'static str,
    func: fn(&mut super::eval::Context, Vec<Value>) -> EvalResult,
    min_args: u16,
    max_args: Option<u16>,
) {
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(name, func, min_args, max_args),
    );
}

/// Register all builtins via defsubr — function pointer dispatch.
///
/// This replaces the giant match-by-name block in dispatch_builtin.
/// Each registered builtin is called via a direct function pointer,
/// matching GNU Emacs's defsubr/funcall_subr architecture.
pub(crate) fn init_builtins(ctx: &mut super::eval::Context) {
    use super::value::*;
    #[cfg(windows)]
    super::w32::register_builtin_subrs(ctx);
    lcms::register_builtin_subrs(ctx);
    // Diagnostics-only VM-profiler control subrs (feature `vm-profile`).
    #[cfg(feature = "vm-profile")]
    {
        ctx.defsubr(
            "neovm--vm-profile-reset",
            defsubr_vm_profile_reset,
            0,
            Some(0),
        );
        ctx.defsubr(
            "neovm--vm-profile-dump",
            defsubr_vm_profile_dump,
            0,
            Some(1),
        );
    }
    ctx.defsubr(
        "neovm--internal-panic",
        defsubr_neovm_internal_panic,
        0,
        Some(1),
    );
    ctx.defsubr_slice("apply", builtin_apply_slice, 1, None);
    ctx.defsubr_slice("funcall", builtin_funcall_slice, 1, None);
    ctx.defsubr_slice(
        "funcall-interactively",
        builtin_funcall_interactively_slice,
        0,
        None,
    );
    ctx.defsubr(
        "funcall-with-delayed-message",
        builtin_funcall_with_delayed_message,
        3,
        Some(3),
    );
    ctx.defsubr("defalias", builtin_defalias, 2, Some(3));
    ctx.defsubr("provide", builtin_provide, 1, Some(2));
    ctx.defsubr("require", builtin_require, 1, Some(3));
    ctx.defsubr("mapcan", builtin_mapcan, 2, Some(2));
    ctx.defsubr_2("mapcar", builtin_mapcar_2, 2);
    ctx.defsubr_2("mapc", builtin_mapc_2, 2);
    ctx.defsubr("mapconcat", builtin_mapconcat, 2, Some(3));
    ctx.defsubr_slice("sort", builtin_sort_slice, 1, None);
    record_builtin_no_eval_policy("functionp", BuiltinNoEvalPolicy::RequiresEvalState);
    ctx.defsubr_1("functionp", builtin_functionp_1, 1);
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("defvaralias", builtin_defvaralias, 2, Some(3)),
    );
    ctx.defsubr_1("boundp", builtin_boundp_1, 1);
    ctx.defsubr("default-boundp", builtin_default_boundp, 1, Some(1));
    ctx.defsubr(
        "default-toplevel-value",
        builtin_default_toplevel_value,
        1,
        Some(1),
    );
    ctx.defsubr_1("fboundp", builtin_fboundp_1, 1);
    ctx.defsubr(
        "internal-make-var-non-special",
        builtin_internal_make_var_non_special,
        1,
        Some(1),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "indirect-variable",
            builtin_indirect_variable,
            1,
            Some(1),
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("handler-bind-1", builtin_handler_bind_1, 1, None),
    );
    ctx.defsubr_1("symbol-value", builtin_symbol_value_1, 1);
    ctx.defsubr_1("symbol-function", builtin_symbol_function_1, 1);
    ctx.defsubr_2("set", builtin_set_2, 2);
    ctx.defsubr("fset", builtin_fset, 2, Some(2));
    ctx.defsubr("makunbound", builtin_makunbound, 1, Some(1));
    ctx.defsubr("fmakunbound", builtin_fmakunbound, 1, Some(1));
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("macroexpand", builtin_macroexpand, 1, Some(2)),
    );
    ctx.defsubr_2("get", builtin_get_2, 2);
    ctx.defsubr_3("put", builtin_put_3, 3);
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("setplist", builtin_setplist, 2, Some(2)),
    );
    ctx.defsubr("symbol-plist", builtin_symbol_plist_fn, 1, Some(1));
    ctx.defsubr("indirect-function", builtin_indirect_function, 1, Some(2));
    ctx.defsubr("signal", super::errors::builtin_signal, 1, Some(2));
    ctx.defsubr(
        "getenv-internal",
        super::process::builtin_getenv_internal,
        1,
        Some(2),
    );
    ctx.defsubr_1("special-variable-p", builtin_special_variable_p_1, 1);
    ctx.defsubr("intern", builtin_intern_fn, 1, Some(2));
    ctx.defsubr("intern-soft", builtin_intern_soft, 1, Some(2));
    ctx.defsubr("run-hook-with-args", builtin_run_hook_with_args, 1, None);
    ctx.defsubr(
        "run-hook-with-args-until-success",
        builtin_run_hook_with_args_until_success,
        0,
        None,
    );
    ctx.defsubr(
        "run-hook-with-args-until-failure",
        builtin_run_hook_with_args_until_failure,
        1,
        None,
    );
    ctx.defsubr("run-hook-wrapped", builtin_run_hook_wrapped, 2, None);
    ctx.defsubr(
        "run-window-configuration-change-hook",
        hooks::builtin_run_window_configuration_change_hook,
        0,
        None,
    );
    ctx.defsubr(
        "run-window-scroll-functions",
        super::window_cmds::builtin_run_window_scroll_functions,
        0,
        None,
    );
    ctx.defsubr("featurep", builtin_featurep, 1, Some(2));
    ctx.defsubr_interactive(
        "garbage-collect",
        builtin_garbage_collect,
        0,
        Some(0),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    ctx.defsubr_2("eval", builtin_eval_2, 1);
    ctx.defsubr("get-buffer-create", builtin_get_buffer_create, 1, Some(2));
    ctx.defsubr("get-buffer", builtin_get_buffer, 1, Some(1));
    super::neo::terminal::syms_of_terminal(ctx);
    ctx.defsubr(
        "neomacs-surface-create",
        super::shader_surface::builtin_neomacs_surface_create,
        0,
        None,
    );
    ctx.defsubr(
        "neomacs-surface-set-uniform",
        super::shader_surface::builtin_neomacs_surface_set_uniform,
        3,
        Some(3),
    );
    ctx.defsubr(
        "neomacs-surface-destroy",
        super::shader_surface::builtin_neomacs_surface_destroy,
        1,
        Some(1),
    );
    ctx.defsubr(
        "neomacs-surface-available-p",
        super::shader_surface::builtin_neomacs_surface_available_p,
        0,
        Some(0),
    );
    ctx.defsubr(
        "neomacs-frame-shader",
        super::shader_surface::builtin_neomacs_frame_shader,
        1,
        Some(3),
    );
    ctx.defsubr(
        "neomacs-frame-shader-set-uniform",
        super::shader_surface::builtin_neomacs_frame_shader_set_uniform,
        2,
        Some(2),
    );
    ctx.defsubr(
        "make-xwidget",
        super::xwidget::builtin_make_xwidget,
        4,
        Some(7),
    );
    ctx.defsubr("xwidgetp", super::xwidget::builtin_xwidgetp, 1, Some(1));
    ctx.defsubr(
        "xwidget-view-p",
        super::xwidget::builtin_xwidget_view_p,
        1,
        Some(1),
    );
    ctx.defsubr(
        "xwidget-live-p",
        super::xwidget::builtin_xwidget_live_p,
        1,
        Some(1),
    );
    ctx.defsubr(
        "xwidget-info",
        super::xwidget::builtin_xwidget_info,
        1,
        Some(1),
    );
    ctx.defsubr(
        "xwidget-view-info",
        super::xwidget::builtin_xwidget_view_info,
        1,
        Some(1),
    );
    ctx.defsubr(
        "xwidget-view-model",
        super::xwidget::builtin_xwidget_view_model,
        1,
        Some(1),
    );
    ctx.defsubr(
        "xwidget-view-window",
        super::xwidget::builtin_xwidget_view_window,
        1,
        Some(1),
    );
    ctx.defsubr(
        "xwidget-view-lookup",
        super::xwidget::builtin_xwidget_view_lookup,
        2,
        Some(2),
    );
    ctx.defsubr(
        "delete-xwidget-view",
        super::xwidget::builtin_delete_xwidget_view,
        1,
        Some(1),
    );
    ctx.defsubr(
        "xwidget-plist",
        super::xwidget::builtin_xwidget_plist,
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-xwidget-plist",
        super::xwidget::builtin_set_xwidget_plist,
        2,
        Some(2),
    );
    ctx.defsubr(
        "xwidget-buffer",
        super::xwidget::builtin_xwidget_buffer,
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-xwidget-buffer",
        super::xwidget::builtin_set_xwidget_buffer,
        2,
        Some(2),
    );
    ctx.defsubr(
        "xwidget-query-on-exit-flag",
        super::xwidget::builtin_xwidget_query_on_exit_flag,
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-xwidget-query-on-exit-flag",
        super::xwidget::builtin_set_xwidget_query_on_exit_flag,
        2,
        Some(2),
    );
    ctx.defsubr(
        "get-buffer-xwidgets",
        super::xwidget::builtin_get_buffer_xwidgets,
        1,
        Some(1),
    );
    ctx.defsubr(
        "kill-xwidget",
        super::xwidget::builtin_kill_xwidget,
        1,
        Some(1),
    );
    ctx.defsubr(
        "xwidget-resize",
        super::xwidget::builtin_xwidget_resize,
        3,
        Some(3),
    );
    ctx.defsubr(
        "xwidget-size-request",
        super::xwidget::builtin_xwidget_size_request,
        1,
        Some(1),
    );
    ctx.defsubr(
        "xwidget-webkit-uri",
        super::xwidget::builtin_xwidget_webkit_uri,
        1,
        Some(1),
    );
    ctx.defsubr(
        "xwidget-webkit-title",
        super::xwidget::builtin_xwidget_webkit_title,
        1,
        Some(1),
    );
    ctx.defsubr(
        "xwidget-webkit-goto-uri",
        super::xwidget::builtin_xwidget_webkit_goto_uri,
        2,
        Some(2),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "make-indirect-buffer",
            builtin_make_indirect_buffer,
            2,
            Some(4),
        )
        .interactive(super::interactive::BuiltinInteractiveSpec::String(
            "bMake indirect buffer (to buffer): \nBName of indirect buffer: ",
        )),
    );
    ctx.defsubr("find-buffer", builtin_find_buffer, 2, Some(2));
    ctx.defsubr("buffer-live-p", builtin_buffer_live_p, 1, Some(1));
    ctx.defsubr(
        "barf-if-buffer-read-only",
        builtin_barf_if_buffer_read_only,
        0,
        Some(1),
    );
    ctx.defsubr(
        "bury-buffer-internal",
        builtin_bury_buffer_internal,
        1,
        Some(1),
    );
    ctx.defsubr("get-file-buffer", builtin_get_file_buffer, 1, Some(1));
    ctx.defsubr_interactive(
        "kill-buffer",
        builtin_kill_buffer,
        0,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("bKill buffer: "),
    );
    ctx.defsubr("set-buffer", builtin_set_buffer, 1, Some(1));
    ctx.defsubr("current-buffer", builtin_current_buffer, 0, Some(0));
    ctx.defsubr("buffer-name", builtin_buffer_name, 0, Some(1));
    ctx.defsubr("buffer-file-name", builtin_buffer_file_name, 0, Some(1));
    ctx.defsubr("buffer-base-buffer", builtin_buffer_base_buffer, 0, Some(1));
    ctx.defsubr("buffer-last-name", builtin_buffer_last_name, 0, Some(1));
    ctx.defsubr_interactive(
        "rename-buffer",
        builtin_rename_buffer,
        1,
        Some(2),
        super::interactive::BuiltinInteractiveSpec::Form(rename_buffer_interactive_spec),
    );
    ctx.defsubr("buffer-string", builtin_buffer_string, 0, Some(0));
    ctx.defsubr(
        "buffer-line-statistics",
        builtin_buffer_line_statistics,
        0,
        None,
    );
    ctx.defsubr(
        "buffer-text-pixel-size",
        super::xdisp::builtin_buffer_text_pixel_size,
        0,
        Some(4),
    );
    ctx.defsubr_interactive(
        "base64-encode-region",
        super::fns::builtin_base64_encode_region,
        2,
        Some(3),
        super::interactive::BuiltinInteractiveSpec::String("r"),
    );
    ctx.defsubr_interactive(
        "base64-decode-region",
        super::fns::builtin_base64_decode_region,
        0,
        None,
        super::interactive::BuiltinInteractiveSpec::String("r"),
    );
    ctx.defsubr_interactive(
        "base64url-encode-region",
        super::fns::builtin_base64url_encode_region,
        2,
        Some(3),
        super::interactive::BuiltinInteractiveSpec::String("r"),
    );
    ctx.defsubr("md5", super::fns::builtin_md5, 1, Some(5));
    ctx.defsubr("secure-hash", super::fns::builtin_secure_hash, 2, Some(5));
    ctx.defsubr("buffer-hash", super::fns::builtin_buffer_hash, 0, Some(1));
    ctx.defsubr("buffer-substring", builtin_buffer_substring, 2, Some(2));
    ctx.defsubr(
        "compare-buffer-substrings",
        builtin_compare_buffer_substrings,
        6,
        Some(6),
    );
    ctx.defsubr_0("point", builtin_point_0);
    ctx.defsubr_0("point-min", builtin_point_min_0);
    ctx.defsubr_0("point-max", builtin_point_max_0);
    ctx.defsubr_1_interactive(
        "goto-char",
        builtin_goto_char_1,
        1,
        super::interactive::BuiltinInteractiveSpec::Form(goto_char_interactive_spec),
    );
    ctx.defsubr("field-beginning", builtin_field_beginning, 0, Some(3));
    ctx.defsubr("field-end", builtin_field_end, 0, Some(3));
    ctx.defsubr("field-string", builtin_field_string, 0, Some(1));
    ctx.defsubr(
        "field-string-no-properties",
        builtin_field_string_no_properties,
        0,
        Some(1),
    );
    ctx.defsubr("constrain-to-field", builtin_constrain_to_field, 2, Some(5));
    ctx.defsubr("insert", builtin_insert, 0, None);
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "insert-and-inherit",
            builtin_insert_and_inherit,
            0,
            None,
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "insert-before-markers-and-inherit",
            builtin_insert_before_markers_and_inherit,
            0,
            None,
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "insert-buffer-substring",
            builtin_insert_buffer_substring,
            1,
            Some(3),
        ),
    );
    ctx.defsubr_interactive(
        "insert-char",
        builtin_insert_char,
        1,
        Some(3),
        super::interactive::BuiltinInteractiveSpec::Form(insert_char_interactive_spec),
    );
    ctx.defsubr("insert-byte", builtin_insert_byte, 2, Some(3));
    ctx.defsubr(
        "replace-region-contents",
        builtin_replace_region_contents,
        3,
        Some(6),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "set-buffer-multibyte",
            builtin_set_buffer_multibyte,
            1,
            Some(1),
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "kill-all-local-variables",
            builtin_kill_all_local_variables,
            0,
            Some(1),
        ),
    );
    ctx.defsubr("buffer-swap-text", builtin_buffer_swap_text, 1, Some(1));
    ctx.defsubr_interactive(
        "delete-region",
        super::editfns::builtin_delete_region,
        2,
        Some(2),
        super::interactive::BuiltinInteractiveSpec::String("r"),
    );
    ctx.defsubr(
        "delete-and-extract-region",
        super::editfns::builtin_delete_and_extract_region,
        2,
        Some(2),
    );
    ctx.defsubr(
        "subst-char-in-region",
        builtin_subst_char_in_region,
        4,
        Some(5),
    );
    ctx.defsubr("delete-field", builtin_delete_field, 0, Some(1));
    ctx.defsubr(
        "delete-all-overlays",
        builtin_delete_all_overlays,
        0,
        Some(1),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::disabled_command(
            "erase-buffer",
            super::editfns::builtin_erase_buffer,
            0,
            Some(0),
        )
        .interactive(super::interactive::BuiltinInteractiveSpec::String("*")),
    );
    ctx.defsubr_interactive(
        "buffer-enable-undo",
        builtin_buffer_enable_undo,
        0,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    ctx.defsubr("buffer-size", builtin_buffer_size, 0, Some(1));
    ctx.defsubr_interactive(
        "narrow-to-region",
        builtin_narrow_to_region,
        2,
        Some(2),
        super::interactive::BuiltinInteractiveSpec::String("r"),
    );
    ctx.defsubr_interactive(
        "widen",
        builtin_widen,
        0,
        Some(0),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::placeholder(
            "internal--labeled-narrow-to-region",
            builtin_internal_labeled_narrow_to_region,
            3,
            Some(3),
            BuiltinNoEvalPlaceholder::Nil,
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::placeholder(
            "internal--labeled-widen",
            builtin_internal_labeled_widen,
            1,
            Some(1),
            BuiltinNoEvalPlaceholder::Nil,
        ),
    );
    ctx.defsubr("buffer-modified-p", builtin_buffer_modified_p, 0, Some(1));
    ctx.defsubr(
        "set-buffer-modified-p",
        builtin_set_buffer_modified_p,
        1,
        Some(1),
    );
    ctx.defsubr(
        "buffer-modified-tick",
        builtin_buffer_modified_tick,
        0,
        None,
    );
    ctx.defsubr(
        "buffer-chars-modified-tick",
        builtin_buffer_chars_modified_tick,
        0,
        None,
    );
    ctx.defsubr("buffer-list", builtin_buffer_list, 0, Some(1));
    ctx.defsubr("other-buffer", builtin_other_buffer, 0, Some(3));
    ctx.defsubr(
        "generate-new-buffer-name",
        builtin_generate_new_buffer_name,
        1,
        Some(2),
    );
    ctx.defsubr("char-after", builtin_char_after, 0, Some(1));
    ctx.defsubr("char-before", builtin_char_before, 0, Some(1));
    ctx.defsubr("byte-to-position", builtin_byte_to_position, 1, Some(1));
    ctx.defsubr("position-bytes", builtin_position_bytes, 1, Some(1));
    ctx.defsubr("get-byte", builtin_get_byte, 0, Some(2));
    ctx.defsubr("buffer-local-value", builtin_buffer_local_value, 2, Some(2));
    ctx.defsubr(
        "local-variable-if-set-p",
        builtin_local_variable_if_set_p,
        1,
        Some(2),
    );
    ctx.defsubr(
        "variable-binding-locus",
        builtin_variable_binding_locus,
        1,
        Some(1),
    );
    ctx.defsubr("interactive-form", builtin_interactive_form, 1, Some(1));
    ctx.defsubr(
        "command-modes",
        super::interactive::builtin_command_modes,
        1,
        Some(1),
    );
    ctx.defsubr_interactive(
        "search-forward",
        builtin_search_forward,
        1,
        Some(4),
        super::interactive::BuiltinInteractiveSpec::String("MSearch: "),
    );
    ctx.defsubr_interactive(
        "search-backward",
        builtin_search_backward,
        1,
        Some(4),
        super::interactive::BuiltinInteractiveSpec::String("MSearch backward: "),
    );
    ctx.defsubr_interactive(
        "re-search-forward",
        builtin_re_search_forward,
        1,
        Some(4),
        super::interactive::BuiltinInteractiveSpec::String("sRE search: "),
    );
    ctx.defsubr_interactive(
        "re-search-backward",
        builtin_re_search_backward,
        1,
        Some(4),
        super::interactive::BuiltinInteractiveSpec::String("sRE search backward: "),
    );
    ctx.defsubr("looking-at", builtin_looking_at, 1, Some(2));
    ctx.defsubr("posix-looking-at", builtin_posix_looking_at, 1, Some(2));
    ctx.defsubr_slice("string-match", builtin_string_match_slice, 2, Some(4));
    // `string-match-p' is NOT here: GNU DEFUNs `string-match'
    // (src/search.c:442) and writes `string-match-p' as a `defsubst' over
    // it (lisp/subr.el:5941), so a compiled caller INLINES
    // `(string-match REGEXP STRING START t)' and never reads the cell
    // (DIVERGENCES.md 152).
    ctx.defsubr("posix-string-match", builtin_posix_string_match, 2, Some(4));
    ctx.defsubr("match-beginning", builtin_match_beginning, 1, Some(1));
    ctx.defsubr("match-end", builtin_match_end, 1, Some(1));
    ctx.defsubr("match-data", builtin_match_data, 0, Some(3));
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "match-data--translate",
            builtin_match_data_translate,
            1,
            Some(1),
        ),
    );
    ctx.defsubr("set-match-data", builtin_set_match_data, 1, Some(2));
    ctx.defsubr("replace-match", builtin_replace_match, 1, Some(5));
    ctx.defsubr(
        "find-charset-region",
        super::charset::builtin_find_charset_region,
        0,
        None,
    );
    ctx.defsubr(
        "charset-after",
        super::charset::builtin_charset_after,
        0,
        Some(1),
    );
    ctx.defsubr(
        "format-mode-line",
        super::xdisp::builtin_format_mode_line_ctx,
        1,
        Some(4),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::placeholder(
            "window-line-height",
            super::xdisp::builtin_window_line_height,
            0,
            Some(2),
            BuiltinNoEvalPlaceholder::WindowLineHeight,
        ),
    );
    ctx.defsubr(
        "posn-at-point",
        super::xdisp::builtin_posn_at_point,
        0,
        Some(2),
    );
    ctx.defsubr("posn-at-x-y", super::xdisp::builtin_posn_at_x_y, 2, Some(4));
    ctx.defsubr(
        "coordinates-in-window-p",
        super::window_cmds::builtin_coordinates_in_window_p,
        2,
        Some(2),
    );
    ctx.defsubr(
        "tool-bar-height",
        super::xdisp::builtin_tool_bar_height_ctx,
        0,
        Some(2),
    );
    ctx.defsubr(
        "tab-bar-height",
        super::xdisp::builtin_tab_bar_height_ctx,
        0,
        Some(2),
    );
    ctx.defsubr("list-fonts", super::font::builtin_list_fonts, 1, Some(4));
    ctx.defsubr("find-font", super::font::builtin_find_font, 1, Some(2));
    ctx.defsubr(
        "font-family-list",
        super::font::builtin_font_family_list,
        0,
        Some(1),
    );
    ctx.defsubr("font-info", super::font::builtin_font_info, 1, Some(2));
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("new-fontset", builtin_new_fontset, 2, Some(2)),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "set-fontset-font",
            builtin_set_fontset_font,
            3,
            Some(5),
        ),
    );
    ctx.defsubr(
        "insert-file-contents",
        super::fileio::builtin_insert_file_contents,
        1,
        Some(5),
    );
    ctx.defsubr_interactive(
        "write-region",
        super::fileio::builtin_write_region,
        3,
        Some(7),
        super::interactive::BuiltinInteractiveSpec::String(
            "r\nFWrite region to file: \ni\ni\ni\np",
        ),
    );
    ctx.defsubr(
        "file-name-completion",
        super::dired::builtin_file_name_completion,
        2,
        Some(3),
    );
    ctx.defsubr(
        "set-visited-file-modtime",
        super::fileio::builtin_set_visited_file_modtime,
        0,
        Some(1),
    );
    ctx.defsubr("make-keymap", builtin_make_keymap, 0, Some(1));
    ctx.defsubr("make-sparse-keymap", builtin_make_sparse_keymap, 0, Some(1));
    ctx.defsubr("copy-keymap", builtin_copy_keymap, 1, Some(1));
    ctx.defsubr("define-key", builtin_define_key, 3, Some(4));
    ctx.defsubr("lookup-key", builtin_lookup_key, 2, Some(3));
    // `global-set-key' (lisp/subr.el:1545) and `local-set-key' (:1569) are
    // NOT here: GNU has no C version of either.  Both are Lisp over
    // `define-key' + `current-global-map' / `current-local-map', which ARE
    // registered just above (DIVERGENCES.md 152).
    ctx.defsubr("use-local-map", builtin_use_local_map, 1, Some(1));
    ctx.defsubr("use-global-map", builtin_use_global_map, 1, Some(1));
    ctx.defsubr("current-local-map", builtin_current_local_map, 0, Some(0));
    ctx.defsubr("current-global-map", builtin_current_global_map, 0, Some(0));
    ctx.defsubr(
        "current-active-maps",
        builtin_current_active_maps,
        0,
        Some(2),
    );
    ctx.defsubr(
        "current-minor-mode-maps",
        builtin_current_minor_mode_maps,
        0,
        Some(0),
    );
    ctx.defsubr("keymap-parent", builtin_keymap_parent, 1, Some(1));
    ctx.defsubr("set-keymap-parent", builtin_set_keymap_parent, 2, Some(2));
    ctx.defsubr("keymapp", builtin_keymapp, 1, Some(1));
    ctx.defsubr("accessible-keymaps", builtin_accessible_keymaps, 1, Some(2));
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("map-keymap", builtin_map_keymap, 2, Some(3)),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "map-keymap-internal",
            builtin_map_keymap_internal,
            2,
            Some(2),
        ),
    );
    ctx.defsubr(
        "print--preprocess",
        super::process::builtin_print_preprocess,
        1,
        Some(1),
    );
    ctx.defsubr(
        "format-network-address",
        super::process::builtin_format_network_address,
        1,
        Some(2),
    );
    ctx.defsubr(
        "network-interface-list",
        super::process::builtin_network_interface_list,
        0,
        Some(2),
    );
    ctx.defsubr(
        "network-interface-info",
        super::process::builtin_network_interface_info,
        1,
        Some(1),
    );
    ctx.defsubr(
        "signal-names",
        super::process::builtin_signal_names,
        0,
        Some(0),
    );
    ctx.defsubr(
        "accept-process-output",
        super::process::builtin_accept_process_output,
        0,
        Some(4),
    );
    ctx.defsubr(
        "list-system-processes",
        super::process::builtin_list_system_processes,
        0,
        Some(0),
    );
    ctx.defsubr(
        "num-processors",
        super::process::builtin_num_processors,
        0,
        Some(1),
    );
    ctx.defsubr(
        "make-process",
        super::process::builtin_make_process,
        0,
        None,
    );
    ctx.defsubr(
        "make-network-process",
        super::process::builtin_make_network_process,
        0,
        None,
    );
    ctx.defsubr(
        "neomacs-open-tls-stream",
        super::process::builtin_neomacs_open_tls_stream,
        4,
        Some(4),
    );
    ctx.defsubr(
        "neomacs-tls-available-p",
        |_ctx, args| super::tls::builtin_neomacs_tls_available_p(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "make-pipe-process",
        super::process::builtin_make_pipe_process,
        0,
        None,
    );
    ctx.defsubr(
        "gnutls-boot",
        super::process::builtin_gnutls_boot,
        3,
        Some(3),
    );
    ctx.defsubr(
        "make-serial-process",
        super::process::builtin_make_serial_process,
        0,
        None,
    );
    ctx.defsubr(
        "serial-process-configure",
        super::process::builtin_serial_process_configure,
        0,
        None,
    );
    ctx.defsubr(
        "call-process",
        super::process::builtin_call_process,
        1,
        None,
    );
    ctx.defsubr(
        "call-process-region",
        super::process::builtin_call_process_region,
        3,
        None,
    );
    ctx.defsubr(
        "continue-process",
        super::process::builtin_continue_process,
        0,
        Some(2),
    );
    ctx.defsubr_interactive(
        "delete-process",
        super::process::builtin_delete_process,
        0,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::Form(delete_process_interactive_spec),
    );
    ctx.defsubr(
        "interrupt-process",
        super::process::builtin_interrupt_process,
        0,
        Some(2),
    );
    ctx.defsubr_interactive(
        "kill-process",
        super::process::builtin_kill_process,
        0,
        Some(2),
        super::interactive::BuiltinInteractiveSpec::Form(kill_process_interactive_spec),
    );
    ctx.defsubr(
        "quit-process",
        super::process::builtin_quit_process,
        0,
        Some(2),
    );
    ctx.defsubr_interactive(
        "signal-process",
        super::process::builtin_signal_process,
        2,
        Some(3),
        super::interactive::BuiltinInteractiveSpec::Form(signal_process_interactive_spec),
    );
    ctx.defsubr(
        "stop-process",
        super::process::builtin_stop_process,
        0,
        Some(2),
    );
    ctx.defsubr(
        "get-process",
        super::process::builtin_get_process,
        1,
        Some(1),
    );
    ctx.defsubr(
        "get-buffer-process",
        super::process::builtin_get_buffer_process,
        1,
        Some(1),
    );
    ctx.defsubr(
        "process-attributes",
        super::process::builtin_process_attributes,
        1,
        Some(1),
    );
    // No `start-process' / `start-file-process' /
    // `start-process-shell-command' / `start-file-process-shell-command':
    // GNU has no C DEFUN for any of them.  All four are Lisp over
    // `make-process' -- lisp/subr.el:3466, lisp/simple.el:5249,
    // lisp/subr.el:5063 and lisp/subr.el:5076 -- and `loadup.el' preloads
    // both files, so a Rust subr here could only ever answer in unit tests.
    // DIVERGENCES.md 149.

    ctx.defsubr("processp", super::process::builtin_processp, 1, Some(1));
    ctx.defsubr("process-id", super::process::builtin_process_id, 1, Some(1));
    ctx.defsubr(
        "process-command",
        super::process::builtin_process_command,
        1,
        Some(1),
    );
    ctx.defsubr(
        "process-contact",
        super::process::builtin_process_contact,
        1,
        Some(3),
    );
    ctx.defsubr(
        "process-filter",
        super::process::builtin_process_filter,
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-process-filter",
        super::process::builtin_set_process_filter,
        2,
        Some(2),
    );
    ctx.defsubr(
        "process-sentinel",
        super::process::builtin_process_sentinel,
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-process-sentinel",
        super::process::builtin_set_process_sentinel,
        2,
        Some(2),
    );
    ctx.defsubr(
        "process-coding-system",
        super::process::builtin_process_coding_system,
        1,
        Some(1),
    );
    ctx.defsubr(
        "process-datagram-address",
        super::process::builtin_process_datagram_address,
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-process-buffer",
        super::process::builtin_set_process_buffer,
        2,
        Some(2),
    );
    ctx.defsubr(
        "set-process-thread",
        super::process::builtin_set_process_thread,
        2,
        Some(2),
    );
    ctx.defsubr(
        "set-process-window-size",
        super::process::builtin_set_process_window_size,
        3,
        Some(3),
    );
    ctx.defsubr(
        "process-tty-name",
        super::process::builtin_process_tty_name,
        1,
        Some(2),
    );
    ctx.defsubr(
        "process-plist",
        super::process::builtin_process_plist,
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-process-plist",
        super::process::builtin_set_process_plist,
        2,
        Some(2),
    );
    ctx.defsubr(
        "process-mark",
        super::process::builtin_process_mark,
        1,
        Some(1),
    );
    ctx.defsubr(
        "process-type",
        super::process::builtin_process_type,
        1,
        Some(1),
    );
    ctx.defsubr(
        "process-thread",
        super::process::builtin_process_thread,
        1,
        Some(1),
    );
    ctx.defsubr(
        "process-running-child-p",
        super::process::builtin_process_running_child_p,
        0,
        Some(1),
    );
    ctx.defsubr(
        "process-send-region",
        super::process::builtin_process_send_region,
        3,
        Some(3),
    );
    ctx.defsubr(
        "process-send-eof",
        super::process::builtin_process_send_eof,
        0,
        Some(1),
    );
    ctx.defsubr(
        "process-send-string",
        super::process::builtin_process_send_string,
        2,
        Some(2),
    );
    ctx.defsubr(
        "process-status",
        super::process::builtin_process_status,
        1,
        Some(1),
    );
    ctx.defsubr(
        "process-exit-status",
        super::process::builtin_process_exit_status,
        1,
        Some(1),
    );
    ctx.defsubr(
        "process-list",
        super::process::builtin_process_list,
        0,
        Some(0),
    );
    ctx.defsubr(
        "process-name",
        super::process::builtin_process_name,
        1,
        Some(1),
    );
    ctx.defsubr(
        "process-buffer",
        super::process::builtin_process_buffer,
        1,
        Some(1),
    );
    ctx.defsubr("sleep-for", super::timer::builtin_sleep_for, 1, Some(2));
    // Timer functions (run-at-time, run-with-timer, run-with-idle-timer,
    // cancel-timer, timerp, timer-activate) are NOT C primitives in GNU
    // Emacs — they're defined in timer.el as Elisp functions.
    // The C layer only provides timer-check (in keyboard.rs) which reads
    // timer-list / timer-idle-list and calls timer-event-handler.
    // Registering them as Rust builtins would shadow the Elisp definitions
    // and create an incompatible parallel timer system.
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "add-variable-watcher",
            super::advice::builtin_add_variable_watcher,
            2,
            Some(2),
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "remove-variable-watcher",
            super::advice::builtin_remove_variable_watcher,
            2,
            Some(2),
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "get-variable-watchers",
            super::advice::builtin_get_variable_watchers,
            1,
            Some(1),
        ),
    );
    ctx.defsubr_interactive(
        "modify-syntax-entry",
        super::syntax::builtin_modify_syntax_entry,
        2,
        Some(3),
        super::interactive::BuiltinInteractiveSpec::String(
            "cSet syntax for character: \nsSet syntax for %s to: ",
        ),
    );
    ctx.defsubr(
        "syntax-table",
        super::syntax::builtin_syntax_table,
        0,
        Some(0),
    );
    ctx.defsubr(
        "set-syntax-table",
        super::syntax::builtin_set_syntax_table,
        1,
        Some(1),
    );
    ctx.defsubr(
        "char-syntax",
        super::syntax::builtin_char_syntax,
        1,
        Some(1),
    );
    ctx.defsubr(
        "matching-paren",
        super::syntax::builtin_matching_paren,
        1,
        Some(1),
    );
    ctx.defsubr(
        "forward-comment",
        super::syntax::builtin_forward_comment,
        1,
        Some(1),
    );
    ctx.defsubr(
        "backward-prefix-chars",
        super::syntax::builtin_backward_prefix_chars,
        0,
        Some(0),
    );
    ctx.defsubr_interactive(
        "forward-word",
        super::syntax::builtin_forward_word,
        0,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("^p"),
    );
    ctx.defsubr("scan-lists", super::syntax::builtin_scan_lists, 3, Some(3));
    ctx.defsubr("scan-sexps", super::syntax::builtin_scan_sexps, 2, Some(2));
    ctx.defsubr(
        "parse-partial-sexp",
        super::syntax::builtin_parse_partial_sexp,
        2,
        Some(6),
    );
    ctx.defsubr(
        "skip-syntax-forward",
        super::syntax::builtin_skip_syntax_forward,
        1,
        Some(2),
    );
    ctx.defsubr(
        "skip-syntax-backward",
        super::syntax::builtin_skip_syntax_backward,
        1,
        Some(2),
    );
    ctx.defsubr_interactive(
        "start-kbd-macro",
        super::kmacro::builtin_start_kbd_macro,
        1,
        Some(2),
        super::interactive::BuiltinInteractiveSpec::String("P"),
    );
    ctx.defsubr_interactive(
        "end-kbd-macro",
        super::kmacro::builtin_end_kbd_macro,
        0,
        Some(2),
        super::interactive::BuiltinInteractiveSpec::String("p"),
    );
    ctx.defsubr_interactive(
        "call-last-kbd-macro",
        super::kmacro::builtin_call_last_kbd_macro,
        0,
        Some(2),
        super::interactive::BuiltinInteractiveSpec::String("p"),
    );
    ctx.defsubr(
        "execute-kbd-macro",
        super::kmacro::builtin_execute_kbd_macro,
        1,
        Some(3),
    );
    ctx.defsubr(
        "store-kbd-macro-event",
        super::kmacro::builtin_store_kbd_macro_event,
        1,
        Some(1),
    );
    ctx.defsubr(
        "put-text-property",
        super::textprop::builtin_put_text_property,
        0,
        None,
    );
    ctx.defsubr(
        "get-text-property",
        super::textprop::builtin_get_text_property,
        2,
        Some(3),
    );
    ctx.defsubr(
        "get-char-property",
        super::textprop::builtin_get_char_property,
        2,
        Some(3),
    );
    ctx.defsubr("get-pos-property", builtin_get_pos_property, 2, Some(3));
    ctx.defsubr(
        "add-face-text-property",
        super::textprop::builtin_add_face_text_property,
        3,
        Some(5),
    );
    ctx.defsubr(
        "add-text-properties",
        super::textprop::builtin_add_text_properties,
        3,
        Some(4),
    );
    ctx.defsubr(
        "set-text-properties",
        super::textprop::builtin_set_text_properties,
        3,
        Some(4),
    );
    ctx.defsubr(
        "remove-text-properties",
        super::textprop::builtin_remove_text_properties,
        0,
        None,
    );
    ctx.defsubr(
        "text-properties-at",
        super::textprop::builtin_text_properties_at,
        1,
        Some(2),
    );
    ctx.defsubr(
        "get-display-property",
        super::textprop::builtin_get_display_property,
        2,
        Some(4),
    );
    ctx.defsubr(
        "next-single-char-property-change",
        builtin_next_single_char_property_change,
        2,
        Some(4),
    );
    ctx.defsubr(
        "previous-single-char-property-change",
        builtin_previous_single_char_property_change,
        0,
        None,
    );
    ctx.defsubr(
        "next-property-change",
        super::textprop::builtin_next_property_change,
        1,
        Some(3),
    );
    ctx.defsubr(
        "next-char-property-change",
        builtin_next_char_property_change,
        1,
        Some(2),
    );
    ctx.defsubr(
        "previous-property-change",
        builtin_previous_property_change,
        1,
        Some(3),
    );
    ctx.defsubr(
        "previous-char-property-change",
        builtin_previous_char_property_change,
        1,
        Some(2),
    );
    ctx.defsubr(
        "text-property-any",
        super::textprop::builtin_text_property_any,
        0,
        None,
    );
    ctx.defsubr(
        "text-property-not-all",
        super::textprop::builtin_text_property_not_all,
        0,
        None,
    );
    ctx.defsubr(
        "next-overlay-change",
        super::buffer::builtin_next_overlay_change,
        1,
        Some(1),
    );
    ctx.defsubr(
        "previous-overlay-change",
        super::buffer::builtin_previous_overlay_change,
        1,
        Some(1),
    );
    ctx.defsubr(
        "make-overlay",
        super::buffer::builtin_make_overlay,
        2,
        Some(5),
    );
    ctx.defsubr(
        "delete-overlay",
        super::buffer::builtin_delete_overlay,
        1,
        Some(1),
    );
    ctx.defsubr(
        "overlay-put",
        super::buffer::builtin_overlay_put,
        3,
        Some(3),
    );
    ctx.defsubr(
        "overlay-get",
        super::buffer::builtin_overlay_get,
        2,
        Some(2),
    );
    ctx.defsubr(
        "overlays-at",
        super::buffer::builtin_overlays_at,
        1,
        Some(2),
    );
    ctx.defsubr(
        "overlays-in",
        super::buffer::builtin_overlays_in,
        2,
        Some(2),
    );
    ctx.defsubr(
        "move-overlay",
        super::buffer::builtin_move_overlay,
        3,
        Some(4),
    );
    ctx.defsubr(
        "overlay-start",
        super::buffer::builtin_overlay_start,
        1,
        Some(1),
    );
    ctx.defsubr(
        "overlay-end",
        super::buffer::builtin_overlay_end,
        1,
        Some(1),
    );
    ctx.defsubr(
        "overlay-buffer",
        super::buffer::builtin_overlay_buffer,
        1,
        Some(1),
    );
    ctx.defsubr(
        "overlay-properties",
        super::buffer::builtin_overlay_properties,
        1,
        Some(1),
    );
    ctx.defsubr("overlayp", super::buffer::builtin_overlayp, 1, Some(1));
    ctx.defsubr("bobp", super::navigation::builtin_bobp, 0, Some(0));
    ctx.defsubr("eobp", super::navigation::builtin_eobp, 0, Some(0));
    ctx.defsubr("bolp", super::navigation::builtin_bolp, 0, Some(0));
    ctx.defsubr("eolp", super::navigation::builtin_eolp, 0, Some(0));
    ctx.defsubr("pos-bol", builtin_pos_bol, 0, Some(1));
    ctx.defsubr(
        "line-end-position",
        super::navigation::builtin_line_end_position,
        0,
        Some(1),
    );
    ctx.defsubr("pos-eol", builtin_pos_eol, 0, Some(1));
    ctx.defsubr(
        "line-number-at-pos",
        super::navigation::builtin_line_number_at_pos,
        0,
        Some(2),
    );
    ctx.defsubr_interactive(
        "forward-line",
        super::navigation::builtin_forward_line,
        0,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("^p"),
    );
    ctx.defsubr_interactive(
        "beginning-of-line",
        super::navigation::builtin_beginning_of_line,
        0,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("^p"),
    );
    ctx.defsubr_interactive(
        "end-of-line",
        super::navigation::builtin_end_of_line,
        0,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("^p"),
    );
    ctx.defsubr_interactive(
        "forward-char",
        super::navigation::builtin_forward_char,
        0,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("^p"),
    );
    ctx.defsubr_interactive(
        "backward-char",
        super::navigation::builtin_backward_char,
        0,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("^p"),
    );
    ctx.defsubr(
        "skip-chars-forward",
        super::navigation::builtin_skip_chars_forward,
        1,
        Some(2),
    );
    ctx.defsubr(
        "skip-chars-backward",
        super::navigation::builtin_skip_chars_backward,
        1,
        Some(2),
    );
    ctx.defsubr(
        "mark-marker",
        super::marker::builtin_mark_marker,
        0,
        Some(0),
    );
    ctx.defsubr(
        "region-beginning",
        super::navigation::builtin_region_beginning,
        0,
        Some(0),
    );
    ctx.defsubr(
        "region-end",
        super::navigation::builtin_region_end,
        0,
        Some(0),
    );
    // `transient-mark-mode' the FUNCTION is not here: it is a
    // `define-minor-mode' at lisp/simple.el:7614.  Only the VARIABLE is C
    // (DEFVAR_LISP, src/buffer.c:5835), and that stays (DIVERGENCES.md 152).
    ctx.defsubr_interactive(
        "make-local-variable",
        super::custom::builtin_make_local_variable,
        1,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("vMake Local Variable: "),
    );
    ctx.defsubr(
        "local-variable-p",
        super::custom::builtin_local_variable_p,
        1,
        Some(2),
    );
    ctx.defsubr(
        "buffer-local-variables",
        super::custom::builtin_buffer_local_variables,
        0,
        None,
    );
    ctx.defsubr_interactive(
        "kill-local-variable",
        super::custom::builtin_kill_local_variable,
        0,
        None,
        super::interactive::BuiltinInteractiveSpec::String("vKill Local Variable: "),
    );
    ctx.defsubr(
        "default-value",
        super::custom::builtin_default_value,
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-default",
        super::custom::builtin_set_default,
        2,
        Some(2),
    );
    ctx.defsubr(
        "set-default-toplevel-value",
        builtin_set_default_toplevel_value,
        2,
        Some(2),
    );
    ctx.defsubr("autoload", super::autoload::builtin_autoload, 2, Some(5));
    ctx.defsubr_3(
        "autoload-do-load",
        super::autoload::builtin_autoload_do_load_3,
        1,
    );
    // `symbol-file' is not here: it is a `defun' at lisp/subr.el:3351 that
    // walks `load-history' (DIVERGENCES.md 152).
    ctx.defsubr_interactive(
        "downcase-region",
        super::casefiddle::builtin_downcase_region,
        2,
        Some(3),
        super::interactive::BuiltinInteractiveSpec::Form(region_noncontiguous_interactive_spec),
    );
    ctx.defsubr_interactive(
        "upcase-region",
        super::casefiddle::builtin_upcase_region,
        2,
        Some(3),
        super::interactive::BuiltinInteractiveSpec::Form(region_noncontiguous_interactive_spec),
    );
    ctx.defsubr_interactive(
        "capitalize-region",
        super::casefiddle::builtin_capitalize_region,
        2,
        Some(3),
        super::interactive::BuiltinInteractiveSpec::Form(region_noncontiguous_interactive_spec),
    );
    ctx.defsubr_interactive(
        "downcase-word",
        super::casefiddle::builtin_downcase_word,
        1,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("p"),
    );
    ctx.defsubr_interactive(
        "upcase-word",
        super::casefiddle::builtin_upcase_word,
        1,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("p"),
    );
    ctx.defsubr_interactive(
        "capitalize-word",
        super::casefiddle::builtin_capitalize_word,
        1,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("p"),
    );
    super::indent::syms_of_indent(ctx);
    ctx.defsubr(
        "selected-window",
        super::window_cmds::builtin_selected_window,
        0,
        Some(0),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "old-selected-window",
            super::window_cmds::builtin_old_selected_window,
            0,
            Some(0),
        ),
    );
    ctx.defsubr(
        "minibuffer-window",
        super::window_cmds::builtin_minibuffer_window,
        0,
        Some(1),
    );
    ctx.defsubr(
        "window-parameter",
        super::window_cmds::builtin_window_parameter,
        2,
        Some(2),
    );
    ctx.defsubr(
        "set-window-parameter",
        super::window_cmds::builtin_set_window_parameter,
        3,
        Some(3),
    );
    ctx.defsubr(
        "window-parameters",
        super::window_cmds::builtin_window_parameters,
        0,
        None,
    );
    ctx.defsubr(
        "window-parent",
        super::window_cmds::builtin_window_parent,
        0,
        Some(1),
    );
    ctx.defsubr(
        "window-top-child",
        super::window_cmds::builtin_window_top_child,
        0,
        Some(1),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "window-left-child",
            super::window_cmds::builtin_window_left_child,
            0,
            Some(1),
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "window-next-sibling",
            super::window_cmds::builtin_window_next_sibling,
            0,
            Some(1),
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "window-prev-sibling",
            super::window_cmds::builtin_window_prev_sibling,
            0,
            Some(1),
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "window-normal-size",
            super::window_cmds::builtin_window_normal_size,
            0,
            Some(2),
        ),
    );
    ctx.defsubr(
        "window-display-table",
        super::window_cmds::builtin_window_display_table,
        0,
        None,
    );
    ctx.defsubr(
        "window-cursor-type",
        super::window_cmds::builtin_window_cursor_type,
        0,
        None,
    );
    ctx.defsubr(
        "window-buffer",
        super::window_cmds::builtin_window_buffer,
        0,
        Some(1),
    );
    ctx.defsubr(
        "window-start",
        super::window_cmds::builtin_window_start,
        0,
        Some(1),
    );
    ctx.defsubr(
        "window-end",
        super::window_cmds::builtin_window_end,
        0,
        Some(2),
    );
    ctx.defsubr(
        "window-point",
        super::window_cmds::builtin_window_point,
        0,
        Some(1),
    );
    ctx.defsubr(
        "window-use-time",
        super::window_cmds::builtin_window_use_time,
        0,
        Some(1),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "window-bump-use-time",
            super::window_cmds::builtin_window_bump_use_time,
            0,
            Some(1),
        ),
    );
    ctx.defsubr(
        "window-old-point",
        super::window_cmds::builtin_window_old_point,
        0,
        Some(1),
    );
    ctx.defsubr(
        "window-old-buffer",
        super::window_cmds::builtin_window_old_buffer,
        0,
        Some(1),
    );
    ctx.defsubr(
        "window-prev-buffers",
        super::window_cmds::builtin_window_prev_buffers,
        0,
        None,
    );
    ctx.defsubr(
        "window-next-buffers",
        super::window_cmds::builtin_window_next_buffers,
        0,
        None,
    );
    ctx.defsubr(
        "window-left-column",
        super::window_cmds::builtin_window_left_column,
        0,
        Some(1),
    );
    ctx.defsubr(
        "window-top-line",
        super::window_cmds::builtin_window_top_line,
        0,
        Some(1),
    );
    ctx.defsubr(
        "window-pixel-left",
        super::window_cmds::builtin_window_pixel_left,
        0,
        Some(1),
    );
    ctx.defsubr(
        "window-pixel-top",
        super::window_cmds::builtin_window_pixel_top,
        0,
        Some(1),
    );
    ctx.defsubr(
        "window-hscroll",
        super::window_cmds::builtin_window_hscroll,
        0,
        Some(1),
    );
    ctx.defsubr(
        "window-vscroll",
        super::window_cmds::builtin_window_vscroll,
        0,
        Some(2),
    );
    ctx.defsubr(
        "window-margins",
        super::window_cmds::builtin_window_margins,
        0,
        None,
    );
    ctx.defsubr(
        "window-fringes",
        super::window_cmds::builtin_window_fringes,
        0,
        None,
    );
    ctx.defsubr(
        "window-scroll-bars",
        super::window_cmds::builtin_window_scroll_bars,
        0,
        None,
    );
    ctx.defsubr(
        "window-pixel-height",
        super::window_cmds::builtin_window_pixel_height,
        0,
        Some(1),
    );
    ctx.defsubr(
        "window-pixel-width",
        super::window_cmds::builtin_window_pixel_width,
        0,
        Some(1),
    );
    // `window-edges' (lisp/window.el:3839), `window-pixel-edges' (:3922) and
    // `window-absolute-pixel-edges' (:3937) are Lisp and only Lisp: GNU has no
    // DEFUN for any of the three (DIVERGENCES.md 154).  `window-edges' is
    // written over the C primitives registered around here --
    // `window-pixel-left', `window-pixel-top', `window-pixel-width',
    // `window-pixel-height', `window-left-column', `window-top-line',
    // `window-total-width', `window-total-height', `window-body-width',
    // `window-body-height' -- and the other two are one-line wrappers over
    // `window-edges' itself, not over any primitive.
    ctx.defsubr(
        "window-body-height",
        super::window_cmds::builtin_window_body_height,
        0,
        Some(2),
    );
    ctx.defsubr(
        "window-body-width",
        super::window_cmds::builtin_window_body_width,
        0,
        Some(2),
    );
    ctx.defsubr(
        "window-text-height",
        super::window_cmds::builtin_window_text_height,
        0,
        None,
    );
    ctx.defsubr(
        "window-text-width",
        super::window_cmds::builtin_window_text_width,
        0,
        None,
    );
    ctx.defsubr(
        "window-total-height",
        super::window_cmds::builtin_window_total_height,
        0,
        Some(2),
    );
    ctx.defsubr(
        "window-total-width",
        super::window_cmds::builtin_window_total_width,
        0,
        Some(2),
    );
    ctx.defsubr(
        "window-list",
        super::window_cmds::builtin_window_list,
        0,
        Some(3),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "window-list-1",
            super::window_cmds::builtin_window_list_1,
            0,
            Some(3),
        ),
    );
    ctx.defsubr(
        "get-buffer-window",
        super::window_cmds::builtin_get_buffer_window,
        0,
        Some(2),
    );
    ctx.defsubr(
        "window-dedicated-p",
        super::window_cmds::builtin_window_dedicated_p,
        0,
        None,
    );
    ctx.defsubr(
        "window-minibuffer-p",
        super::window_cmds::builtin_window_minibuffer_p,
        0,
        None,
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "window-at",
            super::window_cmds::builtin_window_at,
            2,
            Some(3),
        ),
    );
    ctx.defsubr(
        "window-live-p",
        super::window_cmds::builtin_window_live_p,
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-window-start",
        super::window_cmds::builtin_set_window_start,
        2,
        Some(3),
    );
    ctx.defsubr(
        "set-window-hscroll",
        super::window_cmds::builtin_set_window_hscroll,
        2,
        Some(2),
    );
    ctx.defsubr(
        "set-window-margins",
        super::window_cmds::builtin_set_window_margins,
        0,
        None,
    );
    ctx.defsubr(
        "set-window-fringes",
        super::window_cmds::builtin_set_window_fringes,
        2,
        Some(5),
    );
    ctx.defsubr(
        "set-window-vscroll",
        super::window_cmds::builtin_set_window_vscroll,
        2,
        Some(4),
    );
    ctx.defsubr(
        "set-window-point",
        super::window_cmds::builtin_set_window_point,
        2,
        Some(2),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "split-window-internal",
            super::window_cmds::builtin_split_window_internal,
            4,
            Some(5),
        ),
    );
    // `delete-window' (lisp/window.el:4318), `delete-other-windows' (:4453)
    // and `fit-window-to-buffer' (:10307) are Lisp and only Lisp
    // (DIVERGENCES.md 154).  The C primitives they are written over --
    // `delete-window-internal' and `delete-other-windows-internal'
    // (src/window.c) -- are registered below and stay.
    ctx.defsubr(
        "select-window",
        super::window_cmds::builtin_select_window,
        1,
        Some(2),
    );
    ctx.defsubr_interactive(
        "scroll-up",
        super::window_cmds::builtin_scroll_up,
        0,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("^P"),
    );
    ctx.defsubr_interactive(
        "scroll-down",
        super::window_cmds::builtin_scroll_down,
        0,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("^P"),
    );
    ctx.defsubr_interactive(
        "scroll-left",
        super::window_cmds::builtin_scroll_left,
        0,
        Some(2),
        super::interactive::BuiltinInteractiveSpec::String("^P\np"),
    );
    ctx.defsubr_interactive(
        "scroll-right",
        super::window_cmds::builtin_scroll_right,
        0,
        Some(2),
        super::interactive::BuiltinInteractiveSpec::String("^P\np"),
    );
    ctx.defsubr(
        "window-resize-apply",
        super::window_cmds::builtin_window_resize_apply,
        0,
        Some(2),
    );
    ctx.defsubr_interactive(
        "recenter",
        super::window_cmds::builtin_recenter,
        0,
        Some(2),
        super::interactive::BuiltinInteractiveSpec::String("P\np"),
    );
    ctx.defsubr(
        "next-window",
        super::window_cmds::builtin_next_window,
        0,
        Some(3),
    );
    ctx.defsubr(
        "previous-window",
        super::window_cmds::builtin_previous_window,
        0,
        Some(3),
    );
    ctx.defsubr(
        "set-window-buffer",
        super::window_cmds::builtin_set_window_buffer,
        2,
        Some(3),
    );
    ctx.defsubr(
        "current-window-configuration",
        super::window_cmds::builtin_current_window_configuration,
        0,
        None,
    );
    ctx.defsubr(
        "set-window-configuration",
        super::window_cmds::builtin_set_window_configuration,
        1,
        Some(3),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::placeholder(
            "old-selected-frame",
            builtin_old_selected_frame,
            0,
            Some(0),
            BuiltinNoEvalPlaceholder::Nil,
        ),
    );
    ctx.defsubr(
        "selected-frame",
        super::frame::builtin_selected_frame,
        0,
        Some(0),
    );
    ctx.defsubr(
        "mouse-pixel-position",
        builtin_mouse_pixel_position,
        0,
        Some(0),
    );
    ctx.defsubr("mouse-position", builtin_mouse_position, 0, Some(0));
    register_builtin(
        ctx,
        BuiltinRegistration::placeholder(
            "next-frame",
            builtin_next_frame,
            0,
            Some(2),
            BuiltinNoEvalPlaceholder::Nil,
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::placeholder(
            "previous-frame",
            builtin_previous_frame,
            0,
            Some(2),
            BuiltinNoEvalPlaceholder::Nil,
        ),
    );
    ctx.defsubr_interactive(
        "select-frame",
        super::frame::builtin_select_frame,
        1,
        Some(2),
        super::interactive::BuiltinInteractiveSpec::String("e"),
    );
    ctx.defsubr(
        "last-nonminibuffer-frame",
        super::frame::builtin_selected_frame,
        0,
        None,
    );
    ctx.defsubr(
        "visible-frame-list",
        super::frame::builtin_visible_frame_list,
        0,
        None,
    );
    ctx.defsubr("frame-list", super::frame::builtin_frame_list, 0, None);
    ctx.defsubr(
        "x-create-frame",
        super::window_cmds::builtin_x_create_frame,
        1,
        Some(1),
    );
    ctx.defsubr_interactive(
        "make-frame-visible",
        super::frame::builtin_make_frame_visible,
        0,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    // `make-frame' is lisp/frame.el:1019, not a DEFUN (DIVERGENCES.md 154).
    // It funcalls `frame-creation-function', which on a text terminal reaches
    // `make-terminal-frame' -- that one IS a C DEFUN (src/frame.c) and stays.
    ctx.defsubr_interactive(
        "iconify-frame",
        super::frame::builtin_iconify_frame,
        0,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    ctx.defsubr_interactive(
        "delete-frame",
        super::frame::builtin_delete_frame,
        0,
        Some(2),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    ctx.defsubr(
        "frame-char-height",
        super::frame::builtin_frame_char_height,
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-char-width",
        super::frame::builtin_frame_char_width,
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-native-height",
        super::frame::builtin_frame_native_height,
        0,
        None,
    );
    ctx.defsubr(
        "frame-native-width",
        super::frame::builtin_frame_native_width,
        0,
        None,
    );
    ctx.defsubr(
        "frame-text-cols",
        super::frame::builtin_frame_text_cols,
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-text-height",
        super::frame::builtin_frame_text_height,
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-text-lines",
        super::frame::builtin_frame_text_lines,
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-text-width",
        super::frame::builtin_frame_text_width,
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-total-cols",
        super::frame::builtin_frame_total_cols,
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-total-lines",
        super::frame::builtin_frame_total_lines,
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-position",
        super::frame::builtin_frame_position,
        0,
        None,
    );
    ctx.defsubr(
        "frame-parameters",
        super::frame::builtin_frame_parameters,
        0,
        Some(1),
    );
    ctx.defsubr_interactive(
        "set-frame-height",
        super::frame::builtin_set_frame_height,
        2,
        Some(4),
        super::interactive::BuiltinInteractiveSpec::Form(set_frame_height_interactive_spec),
    );
    ctx.defsubr_interactive(
        "set-frame-width",
        super::frame::builtin_set_frame_width,
        2,
        Some(4),
        super::interactive::BuiltinInteractiveSpec::Form(set_frame_width_interactive_spec),
    );
    ctx.defsubr(
        "set-frame-size",
        super::frame::builtin_set_frame_size,
        3,
        Some(4),
    );
    ctx.defsubr(
        "set-frame-position",
        super::frame::builtin_set_frame_position,
        3,
        Some(3),
    );
    ctx.defsubr(
        "frame-visible-p",
        super::frame::builtin_frame_visible_p,
        0,
        None,
    );
    ctx.defsubr(
        "frame-live-p",
        super::frame::builtin_frame_live_p,
        1,
        Some(1),
    );
    ctx.defsubr(
        "frame-initial-p",
        super::terminal::pure::builtin_frame_initial_p,
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-first-window",
        super::window_cmds::builtin_frame_first_window,
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-root-window",
        super::window_cmds::builtin_frame_root_window,
        0,
        Some(1),
    );
    ctx.defsubr("windowp", super::window_cmds::builtin_windowp, 1, Some(1));
    ctx.defsubr(
        "window-valid-p",
        super::window_cmds::builtin_window_valid_p,
        1,
        Some(1),
    );
    ctx.defsubr("framep", super::frame::builtin_framep, 1, Some(1));
    ctx.defsubr(
        "window-frame",
        super::window_cmds::builtin_window_frame,
        0,
        Some(1),
    );
    ctx.defsubr("frame-id", builtin_frame_id, 0, Some(1));
    ctx.defsubr("frame-root-frame", builtin_frame_root_frame, 0, None);
    ctx.defsubr(
        "x-open-connection",
        super::display::builtin_x_open_connection,
        1,
        Some(3),
    );
    ctx.defsubr(
        "x-get-resource",
        super::display::builtin_x_get_resource,
        2,
        Some(4),
    );
    ctx.defsubr(
        "x-list-fonts",
        super::display::builtin_x_list_fonts,
        1,
        Some(5),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "window-system",
            super::display::builtin_window_system,
            0,
            Some(1),
        ),
    );
    ctx.defsubr("current-idle-time", builtin_current_idle_time, 0, Some(0));
    ctx.defsubr(
        "x-server-version",
        super::display::builtin_x_server_version,
        0,
        Some(1),
    );
    ctx.defsubr(
        "x-server-input-extension-version",
        super::display::builtin_x_server_input_extension_version,
        0,
        None,
    );
    ctx.defsubr(
        "x-server-vendor",
        super::display::builtin_x_server_vendor,
        0,
        Some(1),
    );
    // NO `display-color-cells' here.  It is lisp/frame.el:2966 and NOT a
    // DEFUN, so registering it was a shadow like the seventeen
    // DIVERGENCES.md 154 deleted; it was the eighteenth, held back because our
    // `(load "faces")' reached it before `frame.el' defined it.  The cause was
    // a `background-mode' frame parameter Rust seeded before loadup, which GNU
    // computes after it (DIVERGENCES.md 157).  With the seeding gone the
    // caller is gone, and the two C names its Lisp body dispatches to --
    // `x-display-color-cells' (src/xfns.c:5714) and `tty-display-color-cells'
    // (src/term.c:2226) -- are registered right where they always were.
    ctx.defsubr(
        "x-display-mm-height",
        super::display::builtin_x_display_mm_height,
        0,
        Some(1),
    );
    ctx.defsubr(
        "x-display-mm-width",
        super::display::builtin_x_display_mm_width,
        0,
        Some(1),
    );
    ctx.defsubr(
        "x-display-planes",
        super::display::builtin_x_display_planes,
        0,
        Some(1),
    );
    ctx.defsubr(
        "x-display-screens",
        super::display::builtin_x_display_screens,
        0,
        Some(1),
    );
    ctx.defsubr(
        "x-close-connection",
        super::display::builtin_x_close_connection,
        1,
        Some(1),
    );
    ctx.defsubr(
        "call-interactively",
        super::interactive::builtin_call_interactively,
        1,
        Some(3),
    );
    ctx.defsubr_slice(
        "commandp",
        super::interactive::builtin_commandp_interactive,
        1,
        Some(2),
    );
    ctx.defsubr(
        "command-remapping",
        super::interactive::builtin_command_remapping,
        1,
        Some(3),
    );
    ctx.defsubr_interactive(
        "self-insert-command",
        super::interactive::builtin_self_insert_command,
        1,
        Some(2),
        super::interactive::BuiltinInteractiveSpec::Form(self_insert_command_interactive_spec),
    );
    ctx.defsubr(
        "key-binding",
        super::interactive::builtin_key_binding,
        1,
        Some(4),
    );
    ctx.defsubr(
        "where-is-internal",
        super::interactive::builtin_where_is_internal,
        1,
        Some(5),
    );
    ctx.defsubr(
        "this-command-keys",
        super::interactive::builtin_this_command_keys,
        0,
        Some(0),
    );
    ctx.defsubr_slice("format", builtin_format_slice, 1, None);
    ctx.defsubr_slice("format-message", builtin_format_message_slice, 1, None);
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("message-box", builtin_message_box, 1, None),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("message-or-box", builtin_message_or_box, 1, None),
    );
    ctx.defsubr("current-message", builtin_current_message, 0, Some(0));
    ctx.defsubr(
        "read-from-string",
        super::reader::builtin_read_from_string,
        1,
        Some(3),
    );
    ctx.defsubr("read", super::reader::builtin_read, 0, Some(1));
    ctx.defsubr(
        "read-from-minibuffer",
        super::reader::builtin_read_from_minibuffer,
        1,
        Some(7),
    );
    ctx.defsubr(
        "read-string",
        super::reader::builtin_read_string,
        1,
        Some(5),
    );
    ctx.defsubr(
        "completing-read",
        super::reader::builtin_completing_read,
        2,
        Some(8),
    );
    // `read-number' is not here: it is a `defun' at lisp/subr.el:3725 over
    // `read-from-minibuffer', and GNU's "n" interactive code letter reaches
    // it through the function cell (src/callint.c:645) (DIVERGENCES.md 152).
    ctx.defsubr(
        "read-buffer",
        super::minibuffer::builtin_read_buffer,
        1,
        Some(4),
    );
    ctx.defsubr(
        "read-command",
        super::minibuffer::builtin_read_command,
        1,
        Some(2),
    );
    ctx.defsubr(
        "read-variable",
        super::minibuffer::builtin_read_variable,
        1,
        Some(2),
    );
    ctx.defsubr(
        "try-completion",
        super::minibuffer::builtin_try_completion,
        2,
        Some(3),
    );
    ctx.defsubr(
        "all-completions",
        super::minibuffer::builtin_all_completions,
        2,
        Some(3),
    );
    ctx.defsubr(
        "test-completion",
        super::minibuffer::builtin_test_completion,
        2,
        Some(3),
    );
    ctx.defsubr(
        "completion--flex-cost-gotoh",
        super::minibuffer::builtin_flex_cost_gotoh,
        2,
        Some(2),
    );
    ctx.defsubr(
        "input-pending-p",
        super::reader::builtin_input_pending_p,
        0,
        Some(1),
    );
    ctx.defsubr(
        "discard-input",
        super::reader::builtin_discard_input,
        0,
        Some(0),
    );
    ctx.defsubr(
        "current-input-mode",
        super::reader::builtin_current_input_mode,
        0,
        Some(0),
    );
    ctx.defsubr(
        "set-input-mode",
        super::reader::builtin_set_input_mode,
        3,
        Some(4),
    );
    ctx.defsubr(
        "set-input-interrupt-mode",
        super::reader::builtin_set_input_interrupt_mode,
        1,
        Some(1),
    );
    // Keyboard audit Finding 16: register insert-special-event
    // (mirrors GNU `Finsert_special_event` at
    // `src/keyboard.c:12060`). Routes to the same unread queue
    // helper as `unread-command-events`, since neomacs treats
    // every Lisp-side event push the same way.
    ctx.defsubr(
        "insert-special-event",
        super::reader::builtin_insert_special_event,
        1,
        Some(1),
    );
    ctx.defsubr(
        "read-key-sequence",
        super::reader::builtin_read_key_sequence,
        1,
        Some(6),
    );
    ctx.defsubr(
        "read-key-sequence-vector",
        super::reader::builtin_read_key_sequence_vector,
        1,
        Some(6),
    );
    ctx.defsubr("recent-keys", builtin_recent_keys, 0, Some(1));
    ctx.defsubr(
        "minibufferp",
        super::minibuffer::builtin_minibufferp_ctx,
        0,
        Some(2),
    );
    ctx.defsubr(
        "minibuffer-contents",
        super::minibuffer::builtin_minibuffer_contents_ctx,
        0,
        None,
    );
    ctx.defsubr(
        "minibuffer-contents-no-properties",
        super::minibuffer::builtin_minibuffer_contents_no_properties_ctx,
        0,
        None,
    );
    ctx.defsubr(
        "minibuffer-depth",
        super::minibuffer::builtin_minibuffer_depth_ctx,
        0,
        Some(0),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("princ", builtin_princ, 1, Some(2)),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("prin1", builtin_prin1, 1, Some(3)),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "prin1-to-string",
            builtin_prin1_to_string,
            1,
            Some(3),
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("print", builtin_print, 1, Some(2)),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("terpri", builtin_terpri, 0, Some(2)),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("write-char", builtin_write_char, 1, Some(2)),
    );
    ctx.defsubr(
        "backtrace--locals",
        super::misc::builtin_backtrace_locals,
        1,
        Some(2),
    );
    ctx.defsubr(
        "backtrace-debug",
        super::misc::builtin_backtrace_debug,
        2,
        Some(3),
    );
    ctx.defsubr(
        "backtrace-eval",
        super::misc::builtin_backtrace_eval,
        2,
        Some(3),
    );
    ctx.defsubr(
        "backtrace-frame--internal",
        super::misc::builtin_backtrace_frame_internal,
        3,
        Some(3),
    );
    ctx.defsubr(
        "recursion-depth",
        super::misc::builtin_recursion_depth,
        0,
        Some(0),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("kill-emacs", builtin_kill_emacs, 0, Some(2))
            .interactive(super::interactive::BuiltinInteractiveSpec::String("P")),
    );
    ctx.defsubr_interactive(
        "exit-recursive-edit",
        super::minibuffer::builtin_exit_recursive_edit,
        0,
        Some(0),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    ctx.defsubr_interactive(
        "abort-recursive-edit",
        super::minibuffer::builtin_abort_recursive_edit,
        0,
        Some(0),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    ctx.defsubr(
        "make-thread",
        super::threads::builtin_make_thread,
        1,
        Some(3),
    );
    ctx.defsubr(
        "thread-join",
        super::threads::builtin_thread_join,
        1,
        Some(1),
    );
    ctx.defsubr(
        "thread-yield",
        super::threads::builtin_thread_yield,
        0,
        Some(0),
    );
    ctx.defsubr(
        "thread-name",
        super::threads::builtin_thread_name,
        1,
        Some(1),
    );
    ctx.defsubr(
        "thread-live-p",
        super::threads::builtin_thread_live_p,
        1,
        Some(1),
    );
    ctx.defsubr("threadp", super::threads::builtin_threadp, 1, Some(1));
    ctx.defsubr(
        "thread-signal",
        super::threads::builtin_thread_signal,
        3,
        Some(3),
    );
    ctx.defsubr(
        "current-thread",
        super::threads::builtin_current_thread,
        0,
        Some(0),
    );
    ctx.defsubr(
        "all-threads",
        super::threads::builtin_all_threads,
        0,
        Some(0),
    );
    ctx.defsubr(
        "thread-last-error",
        super::threads::builtin_thread_last_error,
        0,
        Some(1),
    );
    ctx.defsubr("make-mutex", super::threads::builtin_make_mutex, 0, Some(1));
    ctx.defsubr("mutex-name", super::threads::builtin_mutex_name, 1, Some(1));
    ctx.defsubr("mutex-lock", super::threads::builtin_mutex_lock, 1, Some(1));
    ctx.defsubr(
        "mutex-unlock",
        super::threads::builtin_mutex_unlock,
        1,
        Some(1),
    );
    ctx.defsubr("mutexp", super::threads::builtin_mutexp, 1, Some(1));
    ctx.defsubr(
        "make-condition-variable",
        super::threads::builtin_make_condition_variable,
        1,
        Some(2),
    );
    ctx.defsubr(
        "condition-variable-p",
        super::threads::builtin_condition_variable_p,
        0,
        None,
    );
    ctx.defsubr(
        "condition-name",
        super::threads::builtin_condition_name,
        1,
        Some(1),
    );
    ctx.defsubr(
        "condition-mutex",
        super::threads::builtin_condition_mutex,
        1,
        Some(1),
    );
    ctx.defsubr(
        "condition-wait",
        super::threads::builtin_condition_wait,
        1,
        Some(1),
    );
    ctx.defsubr(
        "condition-notify",
        super::threads::builtin_condition_notify,
        1,
        Some(2),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "undo-boundary",
            super::undo::builtin_undo_boundary,
            0,
            Some(0),
        ),
    );
    // No `undo' and no `buffer-disable-undo' subr: GNU has neither in C.
    // `syms_of_undo' (src/undo.c:423-490) registers only `&Sundo_boundary'
    // (:435); `undo' is (defun undo (&optional arg) ...) at
    // lisp/simple.el:3466 and `buffer-disable-undo' is
    // (defun buffer-disable-undo (&optional buffer) ...) at
    // lisp/simple.el:3591.  Its partner `buffer-enable-undo' IS in C
    // (src/buffer.c:1829) and is registered above -- the pair is asymmetric
    // in GNU, and copying that asymmetry is the point.  DIVERGENCES.md 150.
    ctx.defsubr("maphash", super::hashtab::builtin_maphash, 2, Some(2));
    ctx.defsubr("mapatoms", super::hashtab::builtin_mapatoms, 1, Some(2));
    // GNU `Sunintern` is `2, 2, 0`: the OBARRAY argument is mandatory (it may
    // be nil to default to `obarray`, but it must be supplied).
    ctx.defsubr("unintern", super::hashtab::builtin_unintern, 2, Some(2));
    ctx.defsubr("set-marker", super::marker::builtin_set_marker, 2, Some(3));
    // No `move-marker' here: GNU has no DEFUN of that name.  It is
    // `(defalias 'move-marker #'set-marker)' at lisp/subr.el:2280, so the
    // function cell holds the SYMBOL `set-marker' (DIVERGENCES.md 148).
    ctx.defsubr(
        "marker-position",
        super::marker::builtin_marker_position,
        1,
        Some(1),
    );
    ctx.defsubr(
        "marker-buffer",
        super::marker::builtin_marker_buffer,
        1,
        Some(1),
    );
    ctx.defsubr(
        "copy-marker",
        super::marker::builtin_copy_marker,
        0,
        Some(2),
    );
    ctx.defsubr(
        "point-marker",
        super::marker::builtin_point_marker,
        0,
        Some(0),
    );
    ctx.defsubr(
        "point-min-marker",
        super::marker::builtin_point_min_marker,
        0,
        Some(0),
    );
    ctx.defsubr(
        "point-max-marker",
        super::marker::builtin_point_max_marker,
        0,
        Some(0),
    );
    ctx.defsubr(
        "current-case-table",
        super::casetab::builtin_current_case_table,
        0,
        Some(0),
    );
    ctx.defsubr(
        "standard-case-table",
        super::casetab::builtin_standard_case_table,
        0,
        Some(0),
    );
    ctx.defsubr(
        "set-case-table",
        super::casetab::builtin_set_case_table,
        1,
        Some(1),
    );
    ctx.defsubr(
        "define-category",
        super::category::builtin_define_category,
        2,
        Some(3),
    );
    ctx.defsubr(
        "category-docstring",
        super::category::builtin_category_docstring,
        1,
        Some(2),
    );
    ctx.defsubr(
        "modify-category-entry",
        super::category::builtin_modify_category_entry,
        2,
        Some(4),
    );
    ctx.defsubr(
        "char-category-set",
        super::category::builtin_char_category_set,
        1,
        Some(1),
    );
    ctx.defsubr(
        "category-table",
        super::category::builtin_category_table,
        0,
        Some(0),
    );
    ctx.defsubr(
        "set-category-table",
        super::category::builtin_set_category_table,
        1,
        Some(1),
    );
    ctx.defsubr(
        "map-char-table",
        super::chartable::builtin_map_char_table,
        2,
        Some(2),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("assoc", builtin_assoc, 2, Some(3)),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("plist-member", builtin_plist_member, 2, Some(3)),
    );
    ctx.defsubr(
        "json-parse-buffer",
        super::json::builtin_json_parse_buffer,
        0,
        None,
    );
    ctx.defsubr("json-insert", super::json::builtin_json_insert, 1, None);
    ctx.defsubr(
        "documentation",
        super::doc::builtin_documentation,
        1,
        Some(2),
    );
    ctx.defsubr(
        "documentation-property",
        super::doc::builtin_documentation_property,
        2,
        Some(3),
    );
    ctx.defsubr_interactive(
        "eval-buffer",
        super::lread::builtin_eval_buffer,
        0,
        Some(5),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    ctx.defsubr_interactive(
        "eval-region",
        super::lread::builtin_eval_region,
        2,
        Some(4),
        super::interactive::BuiltinInteractiveSpec::String("r"),
    );
    ctx.defsubr(
        "read-char-exclusive",
        super::lread::builtin_read_char_exclusive,
        0,
        Some(3),
    );
    ctx.defsubr(
        "insert-before-markers",
        builtin_insert_before_markers,
        0,
        None,
    );
    ctx.defsubr_interactive(
        "delete-char",
        super::editfns::builtin_delete_char,
        1,
        Some(2),
        super::interactive::BuiltinInteractiveSpec::String("p\nP"),
    );
    ctx.defsubr_0("following-char", super::editfns::builtin_following_char_0);
    ctx.defsubr(
        "preceding-char",
        |eval, args| super::editfns::builtin_preceding_char(eval, args),
        0,
        Some(0),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "font-at",
            super::font::builtin_font_at,
            1,
            Some(3),
        ),
    );
    ctx.defsubr("face-font", super::xfaces::builtin_face_font, 1, Some(3));
    ctx.defsubr(
        "access-file",
        super::fileio::builtin_access_file,
        2,
        Some(2),
    );
    ctx.defsubr(
        "expand-file-name",
        super::fileio::builtin_expand_file_name,
        1,
        Some(2),
    );
    ctx.defsubr(
        "delete-file-internal",
        super::fileio::builtin_delete_file_internal,
        1,
        Some(1),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "rename-file",
            super::fileio::builtin_rename_file,
            2,
            Some(3),
        )
        .interactive(super::interactive::BuiltinInteractiveSpec::String(
            "fRename file: \nGRename %s to file: \np",
        )),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "copy-file",
            super::fileio::builtin_copy_file,
            2,
            Some(6),
        )
        .interactive(super::interactive::BuiltinInteractiveSpec::String(
            "fCopy file: \nGCopy %s to file: \np\nP",
        )),
    );
    ctx.defsubr_interactive(
        "add-name-to-file",
        super::fileio::builtin_add_name_to_file,
        2,
        Some(3),
        super::interactive::BuiltinInteractiveSpec::String(
            "fAdd name to file: \nGName to add to %s: \np",
        ),
    );
    ctx.defsubr_interactive(
        "make-symbolic-link",
        super::fileio::builtin_make_symbolic_link,
        2,
        Some(3),
        super::interactive::BuiltinInteractiveSpec::String(
            "FMake symbolic link to file: \nGMake symbolic link to file %s: \np",
        ),
    );
    ctx.defsubr(
        "directory-files",
        super::fileio::builtin_directory_files,
        1,
        Some(5),
    );
    ctx.defsubr(
        "file-attributes",
        super::dired::builtin_file_attributes,
        1,
        Some(2),
    );
    ctx.defsubr(
        "file-exists-p",
        super::fileio::builtin_file_exists_p,
        1,
        Some(1),
    );
    ctx.defsubr(
        "file-readable-p",
        super::fileio::builtin_file_readable_p,
        1,
        Some(1),
    );
    ctx.defsubr(
        "file-writable-p",
        super::fileio::builtin_file_writable_p,
        1,
        Some(1),
    );
    ctx.defsubr("file-acl", super::fileio::builtin_file_acl, 1, Some(1));
    ctx.defsubr(
        "file-executable-p",
        super::fileio::builtin_file_executable_p,
        1,
        Some(1),
    );
    ctx.defsubr(
        "file-locked-p",
        super::filelock::builtin_file_locked_p,
        1,
        Some(1),
    );
    ctx.defsubr(
        "file-selinux-context",
        super::fileio::builtin_file_selinux_context,
        0,
        None,
    );
    ctx.defsubr(
        "file-system-info",
        super::fileio::builtin_file_system_info,
        1,
        Some(1),
    );
    ctx.defsubr(
        "file-directory-p",
        super::fileio::builtin_file_directory_p,
        1,
        Some(1),
    );
    ctx.defsubr(
        "file-regular-p",
        super::fileio::builtin_file_regular_p,
        1,
        Some(1),
    );
    ctx.defsubr(
        "file-symlink-p",
        super::fileio::builtin_file_symlink_p,
        1,
        Some(1),
    );
    ctx.defsubr("file-modes", super::fileio::builtin_file_modes, 1, Some(2));
    ctx.defsubr_interactive(
        "set-file-modes",
        super::fileio::builtin_set_file_modes,
        2,
        Some(3),
        super::interactive::BuiltinInteractiveSpec::Form(set_file_modes_interactive_spec),
    );
    ctx.defsubr(
        "set-file-times",
        super::fileio::builtin_set_file_times,
        1,
        Some(3),
    );
    ctx.defsubr(
        "error-message-string",
        super::errors::builtin_error_message_string,
        1,
        Some(1),
    );
    ctx.defsubr("char-equal", builtin_char_equal, 2, Some(2));
    // No `macrop' here: GNU has no DEFUN of that name.  It is a `defun' at
    // lisp/subr.el:4793 over `indirect-function', which IS in C
    // (src/data.c:2557) -- DIVERGENCES.md 148.
    ctx.defsubr(
        "set-process-inherit-coding-system-flag",
        super::process::builtin_set_process_inherit_coding_system_flag,
        2,
        Some(2),
    );
    ctx.defsubr(
        "frame-parameter",
        super::frame::builtin_frame_parameter,
        2,
        Some(2),
    );
    ctx.defsubr(
        "send-string-to-terminal",
        super::dispnew::pure::builtin_send_string_to_terminal,
        1,
        Some(2),
    );
    ctx.defsubr(
        "internal-show-cursor",
        super::dispnew::pure::builtin_internal_show_cursor,
        2,
        Some(2),
    );
    ctx.defsubr(
        "internal-show-cursor-p",
        super::dispnew::pure::builtin_internal_show_cursor_p,
        0,
        None,
    );
    ctx.defsubr(
        "redraw-frame",
        super::dispnew::pure::builtin_redraw_frame,
        0,
        Some(1),
    );
    ctx.defsubr(
        "display-supports-face-attributes-p",
        super::display::builtin_display_supports_face_attributes_p,
        1,
        Some(2),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "terminal-name",
            super::terminal::pure::builtin_terminal_name,
            0,
            Some(1),
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "terminal-live-p",
            super::terminal::pure::builtin_terminal_live_p,
            1,
            Some(1),
        ),
    );
    ctx.defsubr(
        "terminal-parameter",
        super::terminal::pure::builtin_terminal_parameter,
        2,
        Some(2),
    );
    ctx.defsubr(
        "terminal-parameters",
        super::terminal::pure::builtin_terminal_parameters,
        0,
        Some(1),
    );
    ctx.defsubr(
        "set-terminal-parameter",
        super::terminal::pure::builtin_set_terminal_parameter,
        3,
        Some(3),
    );
    ctx.defsubr(
        "tty-type",
        super::terminal::pure::builtin_tty_type,
        0,
        Some(1),
    );
    ctx.defsubr(
        "tty-top-frame",
        super::terminal::pure::builtin_tty_top_frame,
        0,
        Some(1),
    );
    ctx.defsubr(
        "tty-display-color-p",
        super::terminal::pure::builtin_tty_display_color_p,
        0,
        None,
    );
    ctx.defsubr(
        "tty-display-color-cells",
        super::terminal::pure::builtin_tty_display_color_cells,
        0,
        None,
    );
    ctx.defsubr(
        "tty-no-underline",
        super::terminal::pure::builtin_tty_no_underline,
        0,
        Some(1),
    );
    ctx.defsubr(
        "controlling-tty-p",
        super::terminal::pure::builtin_controlling_tty_p,
        0,
        Some(1),
    );
    ctx.defsubr(
        "suspend-tty",
        super::terminal::pure::builtin_suspend_tty,
        0,
        Some(1),
    );
    ctx.defsubr(
        "resume-tty",
        super::terminal::pure::builtin_resume_tty,
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-terminal",
        super::terminal::pure::builtin_frame_terminal,
        0,
        Some(1),
    );
    ctx.defsubr(
        "x-display-monitor-attributes-list",
        super::display::builtin_x_display_monitor_attributes_list,
        0,
        None,
    );
    ctx.defsubr("read-char", super::reader::builtin_read_char, 0, Some(3));
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "minibuffer-innermost-command-loop-p",
            super::minibuffer::builtin_minibuffer_innermost_command_loop_p_ctx,
            0,
            Some(1),
        ),
    );
    ctx.defsubr_interactive(
        "recursive-edit",
        super::minibuffer::builtin_recursive_edit,
        0,
        Some(0),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "find-coding-systems-region-internal",
            super::coding::builtin_find_coding_systems_region_internal,
            2,
            Some(3),
        ),
    );
    ctx.defsubr_interactive(
        "posix-search-forward",
        super::builtins::search::builtin_posix_search_forward,
        1,
        Some(4),
        super::interactive::BuiltinInteractiveSpec::String("sPosix search: "),
    );
    ctx.defsubr_interactive(
        "posix-search-backward",
        super::builtins::search::builtin_posix_search_backward,
        1,
        Some(4),
        super::interactive::BuiltinInteractiveSpec::String("sPosix search backward: "),
    );
    ctx.defsubr("read-event", super::lread::builtin_read_event, 0, Some(3));
    ctx.defsubr("run-hooks", defsubr_run_hooks, 0, None);
    ctx.defsubr("load", defsubr_load, 1, Some(5));
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("message", defsubr_message, 1, None),
    );
    ctx.defsubr(
        "coding-system-aliases",
        defsubr_coding_system_aliases,
        1,
        Some(1),
    );
    ctx.defsubr(
        "coding-system-plist",
        defsubr_coding_system_plist,
        1,
        Some(1),
    );
    ctx.defsubr("coding-system-put", defsubr_coding_system_put, 3, Some(3));
    ctx.defsubr("coding-system-base", defsubr_coding_system_base, 1, Some(1));
    ctx.defsubr(
        "coding-system-eol-type",
        defsubr_coding_system_eol_type,
        0,
        None,
    );
    ctx.defsubr(
        "detect-coding-string",
        defsubr_detect_coding_string,
        1,
        Some(2),
    );
    ctx.defsubr(
        "detect-coding-region",
        defsubr_detect_coding_region,
        2,
        Some(3),
    );
    ctx.defsubr(
        "keyboard-coding-system",
        defsubr_keyboard_coding_system,
        0,
        Some(1),
    );
    ctx.defsubr(
        "terminal-coding-system",
        defsubr_terminal_coding_system,
        0,
        None,
    );
    ctx.defsubr(
        "coding-system-priority-list",
        defsubr_coding_system_priority_list,
        0,
        Some(1),
    );
    ctx.defsubr(
        "integer-or-marker-p",
        |_ctx, args| builtin_integer_or_marker_p(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "number-or-marker-p",
        |_ctx, args| builtin_number_or_marker_p(args),
        0,
        None,
    );
    ctx.defsubr(
        "vector-or-char-table-p",
        |_ctx, args| builtin_vector_or_char_table_p(args),
        0,
        None,
    );
    ctx.defsubr(
        "markerp",
        |_ctx, args| super::marker::builtin_markerp(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "marker-insertion-type",
        |_ctx, args| super::marker::builtin_marker_insertion_type(args),
        0,
        None,
    );
    ctx.defsubr(
        "make-marker",
        |_ctx, args| super::marker::builtin_make_marker(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "bool-vector-p",
        |_ctx, args| super::chartable::builtin_bool_vector_p(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "make-category-set",
        |_ctx, args| super::category::builtin_make_category_set(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "function-equal",
        |_ctx, args| builtin_function_equal(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "module-function-p",
        |_ctx, args| builtin_module_function_p(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "user-ptrp",
        |_ctx, args| builtin_user_ptrp(args),
        1,
        Some(1),
    );
    ctx.defsubr_1("symbol-with-pos-p", builtin_symbol_with_pos_p_1, 1);
    ctx.defsubr_1("symbol-with-pos-pos", builtin_symbol_with_pos_pos_1, 1);
    ctx.defsubr("length<", |_ctx, args| builtin_length_lt(args), 2, Some(2));
    ctx.defsubr("length=", |_ctx, args| builtin_length_eq(args), 2, Some(2));
    ctx.defsubr("length>", |_ctx, args| builtin_length_gt(args), 2, Some(2));
    ctx.defsubr(
        "substring-no-properties",
        |_ctx, args| builtin_substring_no_properties(args),
        1,
        Some(3),
    );
    ctx.defsubr("sqrt", |_ctx, args| builtin_sqrt(args), 1, Some(1));
    ctx.defsubr("sin", |_ctx, args| builtin_sin(args), 1, Some(1));
    ctx.defsubr("cos", |_ctx, args| builtin_cos(args), 1, Some(1));
    ctx.defsubr("tan", |_ctx, args| builtin_tan(args), 1, Some(1));
    ctx.defsubr("asin", |_ctx, args| builtin_asin(args), 1, Some(1));
    ctx.defsubr("acos", |_ctx, args| builtin_acos(args), 1, Some(1));
    ctx.defsubr("atan", |_ctx, args| builtin_atan(args), 1, Some(2));
    ctx.defsubr("exp", |_ctx, args| builtin_exp(args), 1, Some(1));
    ctx.defsubr("log", |_ctx, args| builtin_log(args), 1, Some(2));
    ctx.defsubr("expt", |_ctx, args| builtin_expt(args), 2, Some(2));
    ctx.defsubr("random", |_ctx, args| builtin_random(args), 0, Some(1));
    ctx.defsubr("isnan", |_ctx, args| builtin_isnan(args), 1, Some(1));
    ctx.defsubr(
        "make-string",
        |_ctx, args| builtin_make_string(args),
        2,
        Some(3),
    );
    ctx.defsubr_slice("string", |_ctx, args| builtin_string_slice(args), 0, None);
    ctx.defsubr("string-width", builtin_string_width, 1, Some(3));
    ctx.defsubr("delete", builtin_delete_with_ctx, 2, Some(2));
    ctx.defsubr_2("delq", builtin_delq_2, 2);
    ctx.defsubr("elt", |_ctx, args| builtin_elt(args), 2, Some(2));
    ctx.defsubr_2("memql", builtin_memql_2, 2);
    ctx.defsubr_slice("nconc", builtin_nconc_slice, 0, None);
    ctx.defsubr("identity", |_ctx, args| builtin_identity(args), 1, Some(1));
    ctx.defsubr("ngettext", |_ctx, args| builtin_ngettext(args), 3, Some(3));
    ctx.defsubr(
        "secure-hash-algorithms",
        |_ctx, args| builtin_secure_hash_algorithms(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "prefix-numeric-value",
        |_ctx, args| builtin_prefix_numeric_value(args),
        0,
        None,
    );
    ctx.defsubr("propertize", |_ctx, args| builtin_propertize(args), 1, None);
    ctx.defsubr_1(
        "bare-symbol",
        super::builtins_extra::builtin_bare_symbol_1,
        1,
    );
    ctx.defsubr(
        "capitalize",
        super::casefiddle::builtin_capitalize_in_state,
        1,
        Some(1),
    );
    ctx.defsubr(
        "charsetp",
        |_ctx, args| super::charset::builtin_charsetp(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "charset-plist",
        |_ctx, args| super::charset::builtin_charset_plist(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "define-charset-internal",
        |_ctx, args| super::charset::builtin_define_charset_internal(args),
        17,
        None,
    );
    ctx.defsubr(
        "define-charset-alias",
        |_ctx, args| super::charset::builtin_define_charset_alias(args),
        0,
        None,
    );
    ctx.defsubr(
        "internal-lisp-face-p",
        super::xfaces::builtin_internal_lisp_face_p,
        0,
        None,
    );
    ctx.defsubr(
        "internal-make-lisp-face",
        super::xfaces::builtin_internal_make_lisp_face,
        0,
        None,
    );
    ctx.defsubr(
        "internal-set-lisp-face-attribute",
        super::xfaces::builtin_internal_set_lisp_face_attribute,
        3,
        Some(4),
    );
    ctx.defsubr(
        "string-to-syntax",
        |_ctx, args| super::syntax::builtin_string_to_syntax(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "syntax-class-to-char",
        |_ctx, args| super::syntax::builtin_syntax_class_to_char(args),
        0,
        None,
    );
    ctx.defsubr(
        "copy-syntax-table",
        |_ctx, args| super::syntax::builtin_copy_syntax_table(args),
        0,
        Some(1),
    );
    ctx.defsubr(
        "syntax-table-p",
        |_ctx, args| super::syntax::builtin_syntax_table_p(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "standard-syntax-table",
        |_ctx, args| super::syntax::builtin_standard_syntax_table(args),
        0,
        None,
    );
    ctx.defsubr(
        "current-time",
        super::timefns::builtin_current_time_in_context,
        0,
        Some(0),
    );
    ctx.defsubr(
        "current-cpu-time",
        |_ctx, args| builtin_current_cpu_time(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "get-internal-run-time",
        |_ctx, args| builtin_get_internal_run_time(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "float-time",
        |_ctx, args| super::timefns::builtin_float_time(args),
        0,
        Some(1),
    );
    ctx.defsubr("daemonp", |_ctx, args| builtin_daemonp(args), 0, Some(0));
    ctx.defsubr("daemon-initialized", builtin_daemon_initialized, 0, Some(0));
    ctx.defsubr(
        "flush-standard-output",
        |_ctx, args| builtin_flush_standard_output(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "force-mode-line-update",
        builtin_force_mode_line_update,
        0,
        Some(1),
    );
    ctx.defsubr(
        "invocation-directory",
        builtin_invocation_directory,
        0,
        Some(0),
    );
    ctx.defsubr("invocation-name", builtin_invocation_name, 0, Some(0));
    ctx.defsubr(
        "file-name-directory",
        super::fileio::builtin_file_name_directory,
        0,
        None,
    );
    ctx.defsubr(
        "file-name-nondirectory",
        super::fileio::builtin_file_name_nondirectory,
        0,
        None,
    );
    ctx.defsubr(
        "file-name-as-directory",
        super::fileio::builtin_file_name_as_directory,
        1,
        Some(1),
    );
    ctx.defsubr(
        "directory-file-name",
        super::fileio::builtin_directory_file_name,
        1,
        Some(1),
    );
    ctx.defsubr(
        "file-name-concat",
        |_ctx, args| super::fileio::builtin_file_name_concat(args),
        1,
        None,
    );
    ctx.defsubr(
        "file-name-absolute-p",
        |_ctx, args| super::fileio::builtin_file_name_absolute_p(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "directory-name-p",
        |_ctx, args| super::fileio::builtin_directory_name_p(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "substitute-in-file-name",
        super::fileio::builtin_substitute_in_file_name,
        0,
        None,
    );
    ctx.defsubr(
        "set-file-acl",
        |_ctx, args| super::fileio::builtin_set_file_acl(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "set-file-selinux-context",
        super::fileio::builtin_set_file_selinux_context,
        2,
        Some(2),
    );
    ctx.defsubr(
        "visited-file-modtime",
        super::fileio::builtin_visited_file_modtime,
        0,
        Some(0),
    );
    ctx.defsubr(
        "make-temp-name",
        |ctx, args| super::fileio::builtin_make_temp_name(ctx, args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "next-read-file-uses-dialog-p",
        |_ctx, args| super::fileio::builtin_next_read_file_uses_dialog_p(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "unhandled-file-name-directory",
        super::fileio::builtin_unhandled_file_name_directory_eval,
        1,
        Some(1),
    );
    ctx.defsubr(
        "get-truename-buffer",
        super::buffer::builtin_get_truename_buffer,
        1,
        Some(1),
    );
    ctx.defsubr(
        "single-key-description",
        |_ctx, args| builtin_single_key_description(args),
        1,
        Some(2),
    );
    ctx.defsubr(
        "key-description",
        |_ctx, args| builtin_key_description(args),
        1,
        Some(2),
    );
    ctx.defsubr(
        "event-convert-list",
        |_ctx, args| builtin_event_convert_list(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "text-char-description",
        |_ctx, args| builtin_text_char_description(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-binary-mode",
        |_ctx, args| super::process::builtin_set_binary_mode(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "group-name",
        |_ctx, args| super::editfns::builtin_group_name(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "group-gid",
        |_ctx, args| super::editfns::builtin_group_gid(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "group-real-gid",
        |_ctx, args| super::editfns::builtin_group_real_gid(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "load-average",
        |_ctx, args| super::editfns::builtin_load_average(args),
        0,
        Some(1),
    );
    ctx.defsubr(
        "logcount",
        |_ctx, args| super::editfns::builtin_logcount(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-frame-size-and-position-pixelwise",
        super::frame::builtin_set_frame_size_and_position_pixelwise,
        0,
        None,
    );
    ctx.defsubr(
        "mouse-position-in-root-frame",
        |_ctx, args| builtin_mouse_position_in_root_frame(args),
        0,
        None,
    );
    ctx.defsubr(
        "x-load-color-file",
        |_ctx, args| super::xfaces::builtin_x_load_color_file(args),
        0,
        None,
    );
    ctx.defsubr(
        "define-fringe-bitmap",
        builtin_define_fringe_bitmap,
        2,
        Some(5),
    );
    ctx.defsubr(
        "destroy-fringe-bitmap",
        builtin_destroy_fringe_bitmap,
        1,
        Some(1),
    );
    ctx.defsubr(
        "display--line-is-continued-p",
        |_ctx, args| builtin_display_line_is_continued_p(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "display--update-for-mouse-movement",
        builtin_display_update_for_mouse_movement,
        3,
        Some(3),
    );
    ctx.defsubr_interactive(
        "do-auto-save",
        super::fileio::builtin_do_auto_save,
        0,
        Some(2),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    // `make-auto-save-file-name' is not here: it is a `defun' at
    // lisp/files.el:7699 over `auto-save-file-name-transforms'.  GNU's C
    // side only READS the buffer field (src/fileio.c:6406)
    // (DIVERGENCES.md 152).
    ctx.defsubr(
        "external-debugging-output",
        builtin_external_debugging_output,
        1,
        Some(1),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "describe-buffer-bindings",
            keymaps::builtin_describe_buffer_bindings,
            1,
            Some(3),
        ),
    );
    ctx.defsubr("describe-vector", builtin_describe_vector, 1, Some(2));
    ctx.defsubr(
        "face-attributes-as-vector",
        |_ctx, args| super::xfaces::builtin_face_attributes_as_vector(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "font-face-attributes",
        |_ctx, args| super::font::builtin_font_face_attributes(args),
        1,
        Some(2),
    );
    ctx.defsubr(
        "font-get-glyphs",
        |_ctx, args| builtin_font_get_glyphs(args),
        3,
        Some(4),
    );
    ctx.defsubr(
        "font-get-system-font",
        |_ctx, args| builtin_font_get_system_font(args),
        0,
        None,
    );
    ctx.defsubr(
        "font-get-system-normal-font",
        |_ctx, args| builtin_font_get_system_normal_font(args),
        0,
        None,
    );
    ctx.defsubr(
        "font-has-char-p",
        |_ctx, args| builtin_font_has_char_p(args),
        2,
        Some(3),
    );
    ctx.defsubr(
        "font-match-p",
        |_ctx, args| builtin_font_match_p(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "font-shape-gstring",
        |_ctx, args| builtin_font_shape_gstring(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "font-variation-glyphs",
        |_ctx, args| builtin_font_variation_glyphs(args),
        0,
        None,
    );
    ctx.defsubr(
        "fontset-font",
        |_ctx, args| builtin_fontset_font(args),
        2,
        Some(3),
    );
    ctx.defsubr(
        "fontset-info",
        |_ctx, args| builtin_fontset_info(args),
        1,
        Some(2),
    );
    ctx.defsubr(
        "fontset-list",
        |_ctx, args| builtin_fontset_list(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "frame--set-was-invisible",
        |_ctx, args| builtin_frame_set_was_invisible(args),
        0,
        None,
    );
    ctx.defsubr(
        "frame-after-make-frame",
        |_ctx, args| builtin_frame_after_make_frame(args),
        0,
        None,
    );
    ctx.defsubr(
        "frame-ancestor-p",
        super::frame::builtin_frame_ancestor_p,
        0,
        None,
    );
    ctx.defsubr(
        "frame-bottom-divider-width",
        super::frame::builtin_frame_bottom_divider_width,
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-child-frame-border-width",
        super::frame::builtin_frame_child_frame_border_width,
        0,
        Some(1),
    );
    ctx.defsubr("frame-focus", super::frame::builtin_frame_focus, 0, Some(1));
    ctx.defsubr(
        "frame-font-cache",
        |_ctx, args| builtin_frame_font_cache(args),
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-fringe-width",
        |_ctx, args| builtin_frame_fringe_width(args),
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-internal-border-width",
        super::frame::builtin_frame_internal_border_width,
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-or-buffer-changed-p",
        |_ctx, args| builtin_frame_or_buffer_changed_p(args),
        0,
        None,
    );
    ctx.defsubr("frame-parent", super::frame::builtin_frame_parent, 0, None);
    ctx.defsubr(
        "frame-pointer-visible-p",
        |_ctx, args| builtin_frame_pointer_visible_p(args),
        0,
        None,
    );
    ctx.defsubr(
        "frame-right-divider-width",
        super::frame::builtin_frame_right_divider_width,
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-scale-factor",
        super::frame::builtin_frame_scale_factor,
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-scroll-bar-height",
        |_ctx, args| builtin_frame_scroll_bar_height(args),
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-scroll-bar-width",
        |_ctx, args| builtin_frame_scroll_bar_width(args),
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame-window-state-change",
        super::frame::builtin_frame_window_state_change,
        0,
        None,
    );
    ctx.defsubr(
        "fringe-bitmaps-at-pos",
        super::xdisp::builtin_fringe_bitmaps_at_pos,
        0,
        Some(2),
    );
    record_builtin_no_eval_policy(
        "fringe-bitmaps-at-pos",
        BuiltinNoEvalPolicy::RequiresEvalState,
    );
    ctx.defsubr("gap-position", builtin_gap_position, 0, Some(0));
    record_builtin_no_eval_policy("gap-position", BuiltinNoEvalPolicy::RequiresEvalState);
    ctx.defsubr("gap-size", builtin_gap_size, 0, Some(0));
    record_builtin_no_eval_policy("gap-size", BuiltinNoEvalPolicy::RequiresEvalState);
    ctx.defsubr(
        "garbage-collect-heapsize",
        |_ctx, args| builtin_garbage_collect_heapsize(args),
        0,
        None,
    );
    ctx.defsubr(
        "garbage-collect-maybe",
        builtin_garbage_collect_maybe,
        1,
        Some(1),
    );
    ctx.defsubr(
        "get-unicode-property-internal",
        |_ctx, args| super::chartable::builtin_get_unicode_property_internal(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "gnutls-available-p",
        |_ctx, args| gnutls::builtin_gnutls_available_p(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "gnutls-asynchronous-parameters",
        super::process::builtin_gnutls_asynchronous_parameters,
        2,
        Some(2),
    );
    ctx.defsubr("gnutls-bye", super::process::builtin_gnutls_bye, 2, Some(2));
    ctx.defsubr(
        "gnutls-ciphers",
        |_ctx, args| gnutls::builtin_gnutls_ciphers(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "gnutls-deinit",
        super::process::builtin_gnutls_deinit,
        1,
        Some(1),
    );
    ctx.defsubr(
        "gnutls-digests",
        |_ctx, args| gnutls::builtin_gnutls_digests(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "gnutls-error-fatalp",
        gnutls::builtin_gnutls_error_fatalp_with_ctx,
        1,
        Some(1),
    );
    ctx.defsubr(
        "gnutls-error-string",
        gnutls::builtin_gnutls_error_string_with_ctx,
        1,
        Some(1),
    );
    ctx.defsubr(
        "gnutls-errorp",
        |_ctx, args| gnutls::builtin_gnutls_errorp(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "gnutls-format-certificate",
        |_ctx, args| gnutls::builtin_gnutls_format_certificate(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "gnutls-get-initstage",
        super::process::builtin_gnutls_get_initstage,
        1,
        Some(1),
    );
    ctx.defsubr(
        "gnutls-hash-digest",
        |_ctx, args| gnutls::builtin_gnutls_hash_digest(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "gnutls-hash-mac",
        |_ctx, args| gnutls::builtin_gnutls_hash_mac(args),
        3,
        Some(3),
    );
    ctx.defsubr(
        "gnutls-macs",
        |_ctx, args| gnutls::builtin_gnutls_macs(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "gnutls-peer-status",
        super::process::builtin_gnutls_peer_status,
        1,
        Some(1),
    );
    ctx.defsubr(
        "gnutls-peer-status-warning-describe",
        |_ctx, args| gnutls::builtin_gnutls_peer_status_warning_describe(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "gnutls-symmetric-decrypt",
        |_ctx, args| gnutls::builtin_gnutls_symmetric_decrypt(args),
        4,
        Some(5),
    );
    ctx.defsubr(
        "gnutls-symmetric-encrypt",
        |_ctx, args| gnutls::builtin_gnutls_symmetric_encrypt(args),
        4,
        Some(5),
    );
    ctx.defsubr_interactive(
        "handle-save-session",
        |_ctx, args| builtin_handle_save_session(args),
        1,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("e"),
    );
    ctx.defsubr_interactive(
        "handle-switch-frame",
        |_ctx, args| builtin_handle_switch_frame(args),
        1,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("^e"),
    );
    ctx.defsubr(
        "help--describe-vector",
        keymaps::builtin_help_describe_vector,
        7,
        Some(7),
    );
    ctx.defsubr(
        "init-image-library",
        |_ctx, args| builtin_init_image_library(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "internal--obarray-buckets",
        |_ctx, args| builtin_internal_obarray_buckets(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "internal--set-buffer-modified-tick",
        builtin_internal_set_buffer_modified_tick,
        1,
        Some(2),
    );
    ctx.defsubr(
        "internal--track-mouse",
        builtin_internal_track_mouse,
        1,
        Some(1),
    );
    ctx.defsubr(
        "internal-char-font",
        super::font::builtin_internal_char_font,
        1,
        Some(2),
    );
    ctx.defsubr(
        "internal-complete-buffer",
        builtin_internal_complete_buffer,
        3,
        Some(3),
    );
    ctx.defsubr(
        "internal-describe-syntax-value",
        builtin_internal_describe_syntax_value,
        1,
        Some(1),
    );
    ctx.defsubr(
        "internal-event-symbol-parse-modifiers",
        builtin_internal_event_symbol_parse_modifiers,
        1,
        Some(1),
    );
    ctx.defsubr(
        "internal-handle-focus-in",
        builtin_internal_handle_focus_in,
        1,
        Some(1),
    );
    ctx.defsubr(
        "internal-set-lisp-face-attribute-from-resource",
        builtin_internal_set_lisp_face_attribute_from_resource,
        3,
        Some(4),
    );
    ctx.defsubr(
        "internal-stack-stats",
        |_ctx, args| builtin_internal_stack_stats(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "internal-subr-documentation",
        |_ctx, args| builtin_internal_subr_documentation(args),
        1,
        Some(1),
    );
    // byte-code: mirrors GNU Emacs Fbyte_code (src/bytecode.c).
    // Receives pre-evaluated args (bytestr, vector, maxdepth), decodes
    // the GNU bytecodes, and executes them via the bytecode VM.
    ctx.defsubr(
        "byte-code",
        |ctx, args| {
            crate::emacs_core::builtins::expect_args("byte-code", &args, 3)?;
            let bytestr = args[0];
            let constants_vec = args[1];
            let maxdepth = args[2];

            use crate::emacs_core::bytecode::ByteCodeFunction;
            use crate::emacs_core::bytecode::decode::decode_gnu_bytecode_with_offset_map;
            use crate::emacs_core::value::LambdaParams;

            // Bytecode strings are unibyte and may contain non-UTF-8 bytes.
            let raw_bytes = if let Some(ls) = ctx.lisp_string(bytestr) {
                ls.as_bytes().to_vec()
            } else {
                return Err(super::error::signal(
                    LispCondition::WrongTypeArgument,
                    vec![Value::symbol("stringp"), bytestr],
                ));
            };

            let mut constants: Vec<Value> = match constants_vec.kind() {
                ValueKind::Veclike(VecLikeType::Vector) => {
                    constants_vec.as_vector_data().unwrap().clone()
                }
                _ => {
                    return Err(super::error::signal(
                        LispCondition::WrongTypeArgument,
                        vec![Value::symbol("vectorp"), constants_vec],
                    ));
                }
            };

            for constant in &mut constants {
                *constant = super::builtins::try_convert_nested_compiled_literal(*constant);
            }

            let (ops, gnu_byte_offset_map) =
                decode_gnu_bytecode_with_offset_map(&raw_bytes, &mut constants).map_err(|e| {
                    super::error::signal(
                        "error",
                        vec![Value::string(format!("bytecode decode error: {}", e))],
                    )
                })?;

            let max_stack = match maxdepth.kind() {
                ValueKind::Fixnum(n) => n as u16,
                _ => 16,
            };

            let bc = ByteCodeFunction {
                source_id: super::bytecode::fresh_bytecode_source_id(),
                ops,
                // The instructions above came straight from the sealing decoder;
                // the stack proof is recomputed below once every shape field
                // (params/lexical/arglist/env/max_stack) is in place.
                ops_sealed: true,
                stack_verified: false,
                constants: constants.into(),
                max_stack,
                params: LambdaParams::simple(vec![]),
                arglist: Value::NIL,
                lexical: false,
                env: None,
                gnu_byte_offset_map: Some(gnu_byte_offset_map),
                gnu_bytecode_bytes: None,
                docstring: None,
                doc_form: None,
                interactive: None,
                closure_slot_count: 4,
                extra_slots: Vec::new(),
                #[cfg(feature = "jit")]
                runtime: crate::emacs_core::jit::Runtime::new(),
                lazy_gnu_code: None,
            };

            ctx.refresh_features_from_variable();
            let mut vm = super::bytecode::Vm::from_context(ctx);
            let result = vm.execute(&bc, vec![]);
            ctx.sync_features_variable();
            result
        },
        0,
        None,
    );
    ctx.defsubr_interactive(
        "decode-coding-region",
        crate::encoding::builtin_decode_coding_region,
        3,
        Some(4),
        super::interactive::BuiltinInteractiveSpec::String("r\nzCoding system: "),
    );
    ctx.defsubr(
        "dump-emacs-portable",
        builtin_dump_emacs_portable,
        1,
        Some(2),
    );
    ctx.defsubr(
        "dump-emacs-portable--sort-predicate",
        |_ctx, args| builtin_dump_emacs_portable_sort_predicate(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "dump-emacs-portable--sort-predicate-copied",
        |_ctx, args| builtin_dump_emacs_portable_sort_predicate_copied(args),
        2,
        Some(2),
    );
    // `emacs-repository-get-version' (lisp/version.el:183) and
    // `emacs-repository-get-branch' (:231) are not here.  They were
    // registered as "gap-fill stubs for loadup.el", but loadup loads
    // version.el at :128 and only calls them at :429 (DIVERGENCES.md 152).
    ctx.defsubr_interactive(
        "encode-coding-region",
        crate::encoding::builtin_encode_coding_region,
        3,
        Some(4),
        super::interactive::BuiltinInteractiveSpec::String("r\nzCoding system: "),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "find-operation-coding-system",
            builtin_find_operation_coding_system,
            1,
            None,
        ),
    );
    ctx.defsubr(
        "iso-charset",
        |_ctx, args| builtin_iso_charset(args),
        3,
        Some(3),
    );
    ctx.defsubr("keymap--get-keyelt", builtin_keymap_get_keyelt, 2, Some(2));
    ctx.defsubr("keymap-prompt", builtin_keymap_prompt, 1, Some(1));
    ctx.defsubr_interactive(
        "lower-frame",
        |_ctx, args| builtin_lower_frame(args),
        0,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    ctx.defsubr(
        "lread--substitute-object-in-subtree",
        |_ctx, args| builtin_lread_substitute_object_in_subtree(args),
        3,
        Some(3),
    );
    ctx.defsubr_interactive(
        "malloc-info",
        |_ctx, args| builtin_malloc_info(args),
        0,
        Some(0),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    ctx.defsubr_interactive(
        "malloc-trim",
        |_ctx, args| builtin_malloc_trim(args),
        0,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    ctx.defsubr(
        "make-byte-code",
        |_ctx, args| builtin_make_byte_code(args),
        4,
        None,
    );
    ctx.defsubr(
        "make-char",
        |_ctx, args| charset::builtin_make_char(args),
        1,
        Some(5),
    );
    ctx.defsubr(
        "make-closure",
        |_ctx, args| builtin_make_closure(args),
        1,
        None,
    );
    ctx.defsubr("make-finalizer", builtin_make_finalizer, 1, Some(1));
    ctx.defsubr(
        "marker-last-position",
        |_ctx, args| builtin_marker_last_position(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "make-interpreted-closure",
        |_ctx, args| builtin_make_interpreted_closure(args),
        3,
        Some(5),
    );
    ctx.defsubr(
        "make-record",
        |_ctx, args| builtin_make_record(args),
        3,
        Some(3),
    );
    ctx.defsubr(
        "make-temp-file-internal",
        builtin_make_temp_file_internal,
        4,
        Some(4),
    );
    ctx.defsubr("map-charset-chars", builtin_map_charset_chars, 2, Some(5));
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "mapbacktrace",
            super::misc::builtin_mapbacktrace,
            1,
            Some(2),
        ),
    );
    ctx.defsubr(
        "memory-info",
        |_ctx, args| builtin_memory_info(args),
        0,
        Some(0),
    );
    // `memory-limit' is not here: it is a `defun' at lisp/subr.el:3574 over
    // `process-attributes', which IS registered (src/process.c)
    // (DIVERGENCES.md 152).
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "make-frame-invisible",
            super::frame::builtin_make_frame_invisible,
            0,
            Some(2),
        )
        .interactive(super::interactive::BuiltinInteractiveSpec::String("")),
    );
    ctx.defsubr(
        "menu-bar-menu-at-x-y",
        builtin_menu_bar_menu_at_x_y,
        2,
        Some(3),
    );
    ctx.defsubr(
        "menu-or-popup-active-p",
        |_ctx, args| builtin_menu_or_popup_active_p(args),
        0,
        Some(0),
    );
    ctx.defsubr("module-load", builtin_module_load, 1, Some(1));
    ctx.defsubr(
        "newline-cache-check",
        |_ctx, args| builtin_newline_cache_check(args),
        0,
        Some(1),
    );
    ctx.defsubr(
        "native-comp-available-p",
        |_ctx, args| builtin_native_comp_available_p(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "obarray-clear",
        |_ctx, args| builtin_obarray_clear(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "obarray-make",
        |_ctx, args| builtin_obarray_make(args),
        0,
        Some(1),
    );
    ctx.defsubr("object-intervals", builtin_object_intervals, 1, Some(1));
    ctx.defsubr_interactive(
        "open-dribble-file",
        builtin_open_dribble_file,
        1,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("FOpen dribble file: "),
    );
    ctx.defsubr(
        "open-font",
        |_ctx, args| builtin_open_font(args),
        1,
        Some(3),
    );
    ctx.defsubr(
        "optimize-char-table",
        |_ctx, args| builtin_optimize_char_table(args),
        1,
        Some(2),
    );
    ctx.defsubr(
        "overlay-lists",
        super::buffer::builtin_overlay_lists,
        0,
        Some(0),
    );
    ctx.defsubr(
        "overlay-recenter",
        super::buffer::builtin_overlay_recenter,
        1,
        Some(1),
    );
    ctx.defsubr(
        "pdumper-stats",
        |_ctx, args| builtin_pdumper_stats(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "play-sound-internal",
        |_ctx, args| super::sound::builtin_play_sound_internal(args),
        1,
        Some(1),
    );
    ctx.defsubr("position-symbol", builtin_position_symbol, 2, Some(2));
    ctx.defsubr("profiler-cpu-log", builtin_profiler_cpu_log, 0, Some(0));
    ctx.defsubr(
        "profiler-cpu-running-p",
        builtin_profiler_cpu_running_p,
        0,
        Some(0),
    );
    ctx.defsubr("profiler-cpu-start", builtin_profiler_cpu_start, 1, Some(1));
    ctx.defsubr("profiler-cpu-stop", builtin_profiler_cpu_stop, 0, Some(0));
    ctx.defsubr(
        "profiler-memory-log",
        builtin_profiler_memory_log,
        0,
        Some(0),
    );
    ctx.defsubr(
        "profiler-memory-running-p",
        builtin_profiler_memory_running_p,
        0,
        Some(0),
    );
    ctx.defsubr(
        "profiler-memory-start",
        builtin_profiler_memory_start,
        0,
        Some(0),
    );
    ctx.defsubr(
        "profiler-memory-stop",
        builtin_profiler_memory_stop,
        0,
        Some(0),
    );
    ctx.defsubr(
        "put-unicode-property-internal",
        |_ctx, args| super::chartable::builtin_put_unicode_property_internal(args),
        3,
        Some(3),
    );
    ctx.defsubr(
        "query-font",
        |_ctx, args| builtin_query_font(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "query-fontset",
        |_ctx, args| builtin_query_fontset(args),
        1,
        Some(2),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::placeholder(
            "raise-frame",
            |_ctx, args| builtin_raise_frame(args),
            0,
            Some(1),
            BuiltinNoEvalPlaceholder::Nil,
        )
        .interactive(super::interactive::BuiltinInteractiveSpec::String("")),
    );
    ctx.defsubr(
        "read-positioning-symbols",
        |ctx, args| super::reader::builtin_read_impl(ctx, args, true),
        0,
        Some(1),
    );
    ctx.defsubr(
        "re--describe-compiled",
        |_ctx, args| builtin_re_describe_compiled(args),
        1,
        Some(2),
    );
    ctx.defsubr(
        "recent-auto-save-p",
        super::buffer::builtin_recent_auto_save_p,
        0,
        Some(0),
    );
    ctx.defsubr("redisplay", builtin_redisplay, 0, Some(1));
    ctx.defsubr(
        "neomacs--frame-snapshot",
        super::xdisp::builtin_neomacs_frame_snapshot,
        0,
        Some(2),
    );
    ctx.defsubr(
        "neomacs--write-frame-snapshot",
        super::xdisp::builtin_neomacs_write_frame_snapshot,
        1,
        Some(3),
    );
    ctx.defsubr(
        "neomacs--debug-lose-device",
        super::xdisp::builtin_neomacs_debug_lose_device,
        0,
        Some(0),
    );
    ctx.defsubr("record", |_ctx, args| builtin_record(args), 1, None);
    ctx.defsubr_1("recordp", builtin_recordp_1, 1);
    ctx.defsubr(
        "reconsider-frame-fonts",
        builtin_reconsider_frame_fonts,
        1,
        Some(1),
    );
    ctx.defsubr_interactive(
        "redirect-debugging-output",
        builtin_redirect_debugging_output,
        1,
        Some(2),
        super::interactive::BuiltinInteractiveSpec::String("FDebug output file: \nP"),
    );
    ctx.defsubr(
        "redirect-frame-focus",
        super::frame::builtin_redirect_frame_focus,
        1,
        Some(2),
    );
    ctx.defsubr(
        "remove-pos-from-symbol",
        |_ctx, args| builtin_remove_pos_from_symbol(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "resize-mini-window-internal",
        super::window_cmds::builtin_resize_mini_window_internal,
        1,
        Some(1),
    );
    ctx.defsubr(
        "restore-buffer-modified-p",
        super::buffer::builtin_restore_buffer_modified_p,
        1,
        Some(1),
    );
    ctx.defsubr(
        "set--this-command-keys",
        builtin_set_this_command_keys,
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-buffer-auto-saved",
        super::buffer::builtin_set_buffer_auto_saved,
        0,
        Some(0),
    );
    ctx.defsubr(
        "set-buffer-major-mode",
        builtin_set_buffer_major_mode,
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-buffer-redisplay",
        builtin_set_buffer_redisplay,
        4,
        Some(4),
    );
    ctx.defsubr(
        "set-charset-plist",
        |_ctx, args| builtin_set_charset_plist(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "set-frame-window-state-change",
        super::frame::builtin_set_frame_window_state_change,
        0,
        Some(2),
    );
    ctx.defsubr(
        "set-fringe-bitmap-face",
        builtin_set_fringe_bitmap_face,
        1,
        Some(2),
    );
    ctx.defsubr(
        "set-minibuffer-window",
        builtin_set_minibuffer_window,
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-mouse-pixel-position",
        builtin_set_mouse_pixel_position,
        3,
        Some(3),
    );
    ctx.defsubr("set-mouse-position", builtin_set_mouse_position, 3, Some(3));
    ctx.defsubr(
        "set-window-new-normal",
        super::window_cmds::builtin_set_window_new_normal,
        1,
        Some(2),
    );
    ctx.defsubr(
        "set-window-new-pixel",
        super::window_cmds::builtin_set_window_new_pixel,
        2,
        Some(3),
    );
    ctx.defsubr(
        "set-window-new-total",
        super::window_cmds::builtin_set_window_new_total,
        2,
        Some(3),
    );
    ctx.defsubr(
        "sort-charsets",
        |_ctx, args| builtin_sort_charsets(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "split-char",
        |_ctx, args| super::charset::builtin_split_char(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "string-distance",
        |_ctx, args| builtin_string_distance(args),
        2,
        Some(3),
    );
    ctx.defsubr(
        "subr-native-lambda-list",
        |_ctx, args| builtin_subr_native_lambda_list(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "subr-type",
        |_ctx, args| builtin_subr_type(args),
        1,
        Some(1),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::placeholder(
            "suspend-emacs",
            |_ctx, args| builtin_suspend_emacs(args),
            0,
            Some(1),
            BuiltinNoEvalPlaceholder::Nil,
        )
        .interactive(super::interactive::BuiltinInteractiveSpec::String("")),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::placeholder(
            "thread--blocker",
            super::threads::builtin_thread_blocker,
            1,
            Some(1),
            BuiltinNoEvalPlaceholder::Nil,
        ),
    );
    ctx.defsubr(
        "tool-bar-get-system-style",
        |_ctx, args| builtin_tool_bar_get_system_style(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "tool-bar-pixel-width",
        |_ctx, args| builtin_tool_bar_pixel_width(args),
        0,
        Some(1),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "translate-region-internal",
            crate::emacs_core::editfns::builtin_translate_region_internal,
            3,
            Some(3),
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "transpose-regions",
            builtin_transpose_regions,
            4,
            Some(5),
        )
        .interactive(super::interactive::BuiltinInteractiveSpec::Form(
            transpose_regions_interactive_spec,
        )),
    );
    ctx.defsubr(
        "tty--output-buffer-size",
        |_ctx, args| builtin_tty_output_buffer_size(args),
        0,
        Some(1),
    );
    ctx.defsubr(
        "tty--set-output-buffer-size",
        |_ctx, args| builtin_tty_set_output_buffer_size(args),
        1,
        Some(2),
    );
    ctx.defsubr(
        "tty-display-pixel-height",
        builtin_tty_display_pixel_height,
        0,
        Some(1),
    );
    ctx.defsubr(
        "tty-display-pixel-width",
        builtin_tty_display_pixel_width,
        0,
        Some(1),
    );
    ctx.defsubr(
        "tty-frame-at",
        super::window_cmds::builtin_tty_frame_at,
        2,
        Some(2),
    );
    ctx.defsubr(
        "tty-frame-edges",
        super::window_cmds::builtin_tty_frame_edges,
        0,
        Some(2),
    );
    ctx.defsubr(
        "tty-frame-geometry",
        super::window_cmds::builtin_tty_frame_geometry,
        0,
        Some(1),
    );
    ctx.defsubr(
        "tty-frame-list-z-order",
        super::window_cmds::builtin_tty_frame_list_z_order,
        0,
        None,
    );
    ctx.defsubr(
        "tty-frame-restack",
        |_ctx, args| builtin_tty_frame_restack(args),
        0,
        None,
    );
    ctx.defsubr(
        "tty-suppress-bold-inverse-default-colors",
        |_ctx, args| builtin_tty_suppress_bold_inverse_default_colors(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "unencodable-char-position",
        super::coding::builtin_unencodable_char_position,
        3,
        Some(5),
    );
    ctx.defsubr(
        "unicode-property-table-internal",
        super::chartable::builtin_unicode_property_table_internal,
        1,
        Some(1),
    );
    ctx.defsubr(
        "unify-charset",
        |_ctx, args| super::charset::builtin_unify_charset(args),
        1,
        Some(3),
    );
    ctx.defsubr_interactive(
        "unix-sync",
        |_ctx, args| builtin_unix_sync(args),
        0,
        Some(0),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    ctx.defsubr("value<", builtin_value_lt, 2, Some(2));
    ctx.defsubr(
        "x-begin-drag",
        |_ctx, args| builtin_x_begin_drag(args),
        1,
        Some(6),
    );
    ctx.defsubr(
        "x-double-buffered-p",
        |_ctx, args| builtin_x_double_buffered_p(args),
        0,
        Some(1),
    );
    ctx.defsubr_interactive(
        "x-menu-bar-open-internal",
        |_ctx, args| builtin_x_menu_bar_open_internal(args),
        0,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("i"),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::placeholder(
            "xw-color-defined-p",
            |ctx, args| super::xfaces::builtin_xw_color_defined_p_ctx(ctx, args),
            1,
            Some(2),
            BuiltinNoEvalPlaceholder::Nil,
        ),
    );
    // `color-defined-p' is lisp/faces.el:1923, not a DEFUN (DIVERGENCES.md
    // 154).  Its body dispatches on `display-graphic-p' to `xw-color-defined-p'
    // -- registered immediately above, and a C DEFUN in GNU -- or to
    // `tty-color-translate'.  Registering the graphical arm under the generic
    // name skipped that dispatch.
    register_builtin(
        ctx,
        BuiltinRegistration::placeholder(
            "xw-color-values",
            |ctx, args| super::xfaces::builtin_xw_color_values_ctx(ctx, args),
            1,
            Some(2),
            BuiltinNoEvalPlaceholder::Nil,
        ),
    );
    // `color-values' is lisp/faces.el:1940, not a DEFUN (DIVERGENCES.md 154),
    // and dispatches the same way: `xw-color-values' (above, a C DEFUN) or
    // `tty-color-values'.
    register_builtin(
        ctx,
        BuiltinRegistration::placeholder(
            "xw-display-color-p",
            |ctx, args| builtin_xw_display_color_p_ctx(ctx, args),
            0,
            Some(1),
            BuiltinNoEvalPlaceholder::Nil,
        ),
    );
    ctx.defsubr("inotify-add-watch", builtin_inotify_add_watch, 3, Some(3));
    ctx.defsubr(
        "inotify-rm-watch",
        |_ctx, args| builtin_inotify_rm_watch(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "inotify-valid-p",
        |_ctx, args| builtin_inotify_valid_p(args),
        1,
        Some(1),
    );
    if INOTIFY_FEATURE_AVAILABLE {
        let _ = ctx.provide_value(Value::symbol("inotify"), None);
    }
    ctx.defsubr("lock-buffer", super::filelock::builtin_lock_buffer, 0, None);
    ctx.defsubr("lock-file", super::filelock::builtin_lock_file, 1, Some(1));
    ctx.defsubr_interactive(
        "lossage-size",
        |_ctx, args| builtin_lossage_size(args),
        0,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::Form(lossage_size_interactive_spec),
    );
    ctx.defsubr(
        "unlock-buffer",
        super::filelock::builtin_unlock_buffer,
        0,
        Some(0),
    );
    ctx.defsubr(
        "unlock-file",
        super::filelock::builtin_unlock_file,
        1,
        Some(1),
    );
    ctx.defsubr(
        "window-bottom-divider-width",
        super::window_cmds::builtin_window_bottom_divider_width,
        0,
        Some(1),
    );
    ctx.defsubr(
        "window-lines-pixel-dimensions",
        |_ctx, args| super::window_cmds::builtin_window_lines_pixel_dimensions(args),
        0,
        Some(6),
    );
    ctx.defsubr(
        "window-new-normal",
        super::window_cmds::builtin_window_new_normal,
        0,
        Some(1),
    );
    ctx.defsubr(
        "window-new-pixel",
        super::window_cmds::builtin_window_new_pixel,
        0,
        Some(1),
    );
    ctx.defsubr(
        "window-new-total",
        super::window_cmds::builtin_window_new_total,
        0,
        Some(1),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::placeholder(
            "window-old-body-pixel-height",
            |_ctx, args| super::window_cmds::builtin_window_old_body_pixel_height(args),
            0,
            None,
            BuiltinNoEvalPlaceholder::FixnumZero,
        ),
    );
    ctx.defsubr(
        "window-old-body-pixel-width",
        |_ctx, args| super::window_cmds::builtin_window_old_body_pixel_width(args),
        0,
        None,
    );
    ctx.defsubr(
        "window-old-pixel-height",
        |_ctx, args| super::window_cmds::builtin_window_old_pixel_height(args),
        0,
        None,
    );
    ctx.defsubr(
        "window-old-pixel-width",
        |_ctx, args| super::window_cmds::builtin_window_old_pixel_width(args),
        0,
        None,
    );
    ctx.defsubr(
        "window-right-divider-width",
        super::window_cmds::builtin_window_right_divider_width,
        0,
        Some(1),
    );
    ctx.defsubr(
        "window-scroll-bar-height",
        super::window_cmds::builtin_window_scroll_bar_height,
        0,
        None,
    );
    ctx.defsubr(
        "window-scroll-bar-width",
        super::window_cmds::builtin_window_scroll_bar_width,
        0,
        None,
    );
    ctx.defsubr(
        "treesit-available-p",
        |_ctx, args| builtin_treesit_available_p(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "treesit-compiled-query-p",
        |_ctx, args| builtin_treesit_compiled_query_p(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-induce-sparse-tree",
        builtin_treesit_induce_sparse_tree,
        2,
        Some(4),
    );
    ctx.defsubr(
        "treesit-language-abi-version",
        builtin_treesit_language_abi_version,
        0,
        Some(1),
    );
    ctx.defsubr(
        "treesit-language-available-p",
        builtin_treesit_language_available_p,
        1,
        Some(2),
    );
    ctx.defsubr(
        "treesit-library-abi-version",
        |_ctx, args| builtin_treesit_library_abi_version(args),
        0,
        Some(1),
    );
    ctx.defsubr("treesit-node-check", builtin_treesit_node_check, 2, Some(2));
    ctx.defsubr("treesit-node-child", builtin_treesit_node_child, 2, Some(3));
    ctx.defsubr(
        "treesit-node-child-by-field-name",
        builtin_treesit_node_child_by_field_name,
        2,
        Some(2),
    );
    ctx.defsubr(
        "treesit-node-child-count",
        builtin_treesit_node_child_count,
        1,
        Some(2),
    );
    ctx.defsubr(
        "treesit-node-descendant-for-range",
        builtin_treesit_node_descendant_for_range,
        3,
        Some(4),
    );
    ctx.defsubr("treesit-node-end", builtin_treesit_node_end, 1, Some(1));
    ctx.defsubr("treesit-node-eq", builtin_treesit_node_eq, 2, Some(2));
    ctx.defsubr(
        "treesit-node-field-name-for-child",
        builtin_treesit_node_field_name_for_child,
        2,
        Some(2),
    );
    ctx.defsubr(
        "treesit-node-first-child-for-pos",
        builtin_treesit_node_first_child_for_pos,
        2,
        Some(3),
    );
    ctx.defsubr(
        "treesit-node-match-p",
        builtin_treesit_node_match_p,
        2,
        Some(3),
    );
    ctx.defsubr(
        "treesit-node-next-sibling",
        builtin_treesit_node_next_sibling,
        1,
        Some(2),
    );
    ctx.defsubr(
        "treesit-node-p",
        |_ctx, args| builtin_treesit_node_p(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-node-parent",
        builtin_treesit_node_parent,
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-node-parser",
        |_ctx, args| builtin_treesit_node_parser(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-node-prev-sibling",
        builtin_treesit_node_prev_sibling,
        1,
        Some(2),
    );
    ctx.defsubr("treesit-node-start", builtin_treesit_node_start, 1, Some(1));
    ctx.defsubr(
        "treesit-node-string",
        builtin_treesit_node_string,
        1,
        Some(1),
    );
    ctx.defsubr("treesit-node-type", builtin_treesit_node_type, 1, Some(1));
    ctx.defsubr(
        "treesit-parser-add-notifier",
        builtin_treesit_parser_add_notifier,
        2,
        Some(2),
    );
    ctx.defsubr(
        "treesit-parser-buffer",
        builtin_treesit_parser_buffer,
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-parser-create",
        builtin_treesit_parser_create,
        1,
        Some(4),
    );
    ctx.defsubr(
        "treesit-parser-delete",
        builtin_treesit_parser_delete,
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-parser-included-ranges",
        builtin_treesit_parser_included_ranges,
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-parser-language",
        builtin_treesit_parser_language,
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-parser-list",
        builtin_treesit_parser_list,
        0,
        Some(3),
    );
    ctx.defsubr(
        "treesit-parser-notifiers",
        builtin_treesit_parser_notifiers,
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-parser-p",
        |_ctx, args| builtin_treesit_parser_p(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-parser-remove-notifier",
        builtin_treesit_parser_remove_notifier,
        2,
        Some(2),
    );
    ctx.defsubr(
        "treesit-parser-root-node",
        builtin_treesit_parser_root_node,
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-parser-set-included-ranges",
        builtin_treesit_parser_set_included_ranges,
        2,
        Some(2),
    );
    ctx.defsubr("treesit-parser-tag", builtin_treesit_parser_tag, 1, Some(1));
    ctx.defsubr(
        "treesit-pattern-expand",
        |_ctx, args| builtin_treesit_pattern_expand(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-query-capture",
        builtin_treesit_query_capture,
        2,
        Some(6),
    );
    ctx.defsubr(
        "treesit-query-compile",
        builtin_treesit_query_compile,
        2,
        Some(3),
    );
    ctx.defsubr(
        "treesit-query-expand",
        |_ctx, args| builtin_treesit_query_expand(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-query-language",
        |_ctx, args| builtin_treesit_query_language(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-query-p",
        |_ctx, args| builtin_treesit_query_p(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-search-forward",
        builtin_treesit_search_forward,
        2,
        Some(4),
    );
    ctx.defsubr(
        "treesit-search-subtree",
        builtin_treesit_search_subtree,
        2,
        Some(5),
    );
    ctx.defsubr(
        "treesit-subtree-stat",
        builtin_treesit_subtree_stat,
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-grammar-location",
        builtin_treesit_grammar_location,
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-tracking-line-column-p",
        builtin_treesit_tracking_line_column_p,
        0,
        Some(1),
    );
    ctx.defsubr(
        "treesit-parser-tracking-line-column-p",
        builtin_treesit_parser_tracking_line_column_p,
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-query-eagerly-compiled-p",
        builtin_treesit_query_eagerly_compiled_p,
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-query-source",
        |_ctx, args| builtin_treesit_query_source(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-parser-embed-level",
        builtin_treesit_parser_embed_level,
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit-parser-set-embed-level",
        builtin_treesit_parser_set_embed_level,
        2,
        Some(2),
    );
    ctx.defsubr(
        "treesit-parse-string",
        builtin_treesit_parse_string,
        2,
        Some(2),
    );
    ctx.defsubr(
        "treesit-parser-changed-regions",
        builtin_treesit_parser_changed_regions,
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit--linecol-at",
        builtin_treesit_linecol_at,
        1,
        Some(1),
    );
    ctx.defsubr(
        "treesit--linecol-cache-set",
        builtin_treesit_linecol_cache_set,
        3,
        Some(3),
    );
    ctx.defsubr(
        "treesit--linecol-cache",
        builtin_treesit_linecol_cache,
        0,
        Some(0),
    );
    ctx.defsubr(
        "sqlite-available-p",
        |_ctx, args| super::sqlite::builtin_sqlite_available_p(args),
        0,
        Some(0),
    );
    if super::sqlite::SQLITE3_LISP_API_AVAILABLE {
        ctx.defsubr(
            "sqlite-close",
            |_ctx, args| super::sqlite::builtin_sqlite_close(args),
            1,
            Some(1),
        );
        ctx.defsubr(
            "sqlite-columns",
            |_ctx, args| super::sqlite::builtin_sqlite_columns(args),
            1,
            Some(1),
        );
        ctx.defsubr(
            "sqlite-commit",
            |_ctx, args| super::sqlite::builtin_sqlite_commit(args),
            1,
            Some(1),
        );
        ctx.defsubr(
            "sqlite-execute",
            |_ctx, args| super::sqlite::builtin_sqlite_execute(args),
            2,
            Some(3),
        );
        ctx.defsubr(
            "sqlite-execute-batch",
            super::sqlite::builtin_sqlite_execute_batch,
            2,
            Some(2),
        );
        ctx.defsubr(
            "sqlite-finalize",
            |_ctx, args| super::sqlite::builtin_sqlite_finalize(args),
            1,
            Some(1),
        );
        ctx.defsubr(
            "sqlite-load-extension",
            super::sqlite::builtin_sqlite_load_extension,
            2,
            Some(2),
        );
        ctx.defsubr(
            "sqlite-more-p",
            |_ctx, args| super::sqlite::builtin_sqlite_more_p(args),
            1,
            Some(1),
        );
        ctx.defsubr(
            "sqlite-next",
            |_ctx, args| super::sqlite::builtin_sqlite_next(args),
            1,
            Some(1),
        );
        ctx.defsubr(
            "sqlite-open",
            |_ctx, args| super::sqlite::builtin_sqlite_open(args),
            0,
            Some(3),
        );
        ctx.defsubr(
            "sqlite-pragma",
            |_ctx, args| super::sqlite::builtin_sqlite_pragma(args),
            2,
            Some(2),
        );
        ctx.defsubr(
            "sqlite-rollback",
            |_ctx, args| super::sqlite::builtin_sqlite_rollback(args),
            1,
            Some(1),
        );
        ctx.defsubr(
            "sqlite-select",
            |_ctx, args| super::sqlite::builtin_sqlite_select(args),
            2,
            Some(4),
        );
        ctx.defsubr(
            "sqlite-transaction",
            |_ctx, args| super::sqlite::builtin_sqlite_transaction(args),
            1,
            Some(1),
        );
        ctx.defsubr(
            "sqlite-version",
            |_ctx, args| super::sqlite::builtin_sqlite_version(args),
            0,
            Some(0),
        );
    }
    ctx.defsubr(
        "sqlitep",
        |_ctx, args| super::sqlite::builtin_sqlitep(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "fillarray",
        |_ctx, args| builtin_fillarray(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "define-hash-table-test",
        |_ctx, args| builtin_define_hash_table_test(args),
        3,
        Some(3),
    );
    ctx.defsubr(
        "hash-table-test",
        |_ctx, args| super::hashtab::builtin_hash_table_test(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "hash-table-size",
        |_ctx, args| super::hashtab::builtin_hash_table_size(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "hash-table-rehash-size",
        |_ctx, args| super::hashtab::builtin_hash_table_rehash_size(args),
        0,
        None,
    );
    ctx.defsubr(
        "hash-table-rehash-threshold",
        |_ctx, args| super::hashtab::builtin_hash_table_rehash_threshold(args),
        0,
        None,
    );
    ctx.defsubr(
        "hash-table-weakness",
        |_ctx, args| super::hashtab::builtin_hash_table_weakness(args),
        0,
        None,
    );
    ctx.defsubr(
        "copy-hash-table",
        |_ctx, args| super::hashtab::builtin_copy_hash_table(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "sxhash-eq",
        |_ctx, args| super::hashtab::builtin_sxhash_eq(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "sxhash-eql",
        |_ctx, args| super::hashtab::builtin_sxhash_eql(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "sxhash-equal",
        |_ctx, args| super::hashtab::builtin_sxhash_equal(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "sxhash-equal-including-properties",
        |_ctx, args| super::hashtab::builtin_sxhash_equal_including_properties(args),
        0,
        None,
    );
    ctx.defsubr(
        "internal--hash-table-buckets",
        |_ctx, args| super::hashtab::builtin_internal_hash_table_buckets(args),
        0,
        None,
    );
    ctx.defsubr(
        "internal--hash-table-histogram",
        |_ctx, args| super::hashtab::builtin_internal_hash_table_histogram(args),
        0,
        None,
    );
    ctx.defsubr(
        "internal--hash-table-index-size",
        |_ctx, args| super::hashtab::builtin_internal_hash_table_index_size(args),
        0,
        None,
    );
    ctx.defsubr(
        "neomacs-frame-geometry",
        |_ctx, args| builtin_neomacs_frame_geometry(args),
        0,
        None,
    );
    ctx.defsubr(
        "neomacs-frame-edges",
        super::window_cmds::builtin_neomacs_frame_edges,
        0,
        Some(2),
    );
    ctx.defsubr(
        "neomacs-mouse-absolute-pixel-position",
        |_ctx, args| builtin_neomacs_mouse_absolute_pixel_position(args),
        0,
        None,
    );
    ctx.defsubr(
        "neomacs-set-mouse-absolute-pixel-position",
        |_ctx, args| builtin_neomacs_set_mouse_absolute_pixel_position(args),
        0,
        None,
    );
    ctx.defsubr(
        "neomacs-effect-set",
        effects::builtin_neomacs_effect_set,
        1,
        None,
    );
    ctx.defsubr(
        "neomacs-effect-get",
        effects::builtin_neomacs_effect_get,
        1,
        Some(1),
    );
    ctx.defsubr(
        "neomacs-effect-reset",
        effects::builtin_neomacs_effect_reset,
        1,
        Some(1),
    );
    ctx.defsubr(
        "neomacs-effects-apply",
        effects::builtin_neomacs_effects_apply,
        1,
        Some(1),
    );
    ctx.defsubr(
        "neomacs-effect-names",
        effects::builtin_neomacs_effect_names,
        0,
        Some(1),
    );
    ctx.defsubr(
        "neomacs-display-monitor-attributes-list",
        builtin_neomacs_display_monitor_attributes_list,
        0,
        None,
    );
    ctx.defsubr(
        "neomacs-clipboard-set",
        builtin_neomacs_clipboard_set,
        0,
        None,
    );
    ctx.defsubr(
        "neomacs-clipboard-get",
        builtin_neomacs_clipboard_get,
        0,
        None,
    );
    ctx.defsubr(
        "neomacs-primary-selection-set",
        builtin_neomacs_primary_selection_set,
        0,
        None,
    );
    ctx.defsubr(
        "neomacs-primary-selection-get",
        builtin_neomacs_primary_selection_get,
        0,
        None,
    );
    ctx.defsubr(
        "neomacs-core-backend",
        |_ctx, args| builtin_neomacs_core_backend(args),
        0,
        None,
    );
    ctx.defsubr(
        "neomacs-buffer-text-backend",
        builtin_neomacs_buffer_text_backend,
        0,
        None,
    );
    ctx.defsubr(
        "neomacs-default-buffer-text-backend",
        builtin_neomacs_default_buffer_text_backend,
        0,
        None,
    );
    ctx.defsubr(
        "neomacs-set-default-buffer-text-backend",
        builtin_neomacs_set_default_buffer_text_backend,
        0,
        None,
    );
    ctx.defsubr(
        "neomacs-set-buffer-text-backend",
        builtin_neomacs_set_buffer_text_backend,
        0,
        None,
    );
    ctx.defsubr(
        "buffer-local-toplevel-value",
        super::custom::builtin_buffer_local_toplevel_value,
        0,
        None,
    );
    ctx.defsubr(
        "set-buffer-local-toplevel-value",
        super::custom::builtin_set_buffer_local_toplevel_value,
        0,
        None,
    );
    ctx.defsubr(
        "debugger-trap",
        |_ctx, args| builtin_debugger_trap(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "internal-delete-indirect-variable",
        builtin_internal_delete_indirect_variable,
        0,
        None,
    );
    ctx.defsubr(
        "thread-buffer-disposition",
        super::threads::builtin_thread_buffer_disposition,
        1,
        Some(1),
    );
    ctx.defsubr(
        "thread-set-buffer-disposition",
        super::threads::builtin_thread_set_buffer_disposition,
        2,
        Some(2),
    );
    ctx.defsubr(
        "window-discard-buffer-from-window",
        super::window_cmds::builtin_window_discard_buffer_from_window,
        2,
        Some(3),
    );
    ctx.defsubr(
        "window-cursor-info",
        super::window_cmds::builtin_window_cursor_info,
        0,
        Some(1),
    );
    ctx.defsubr(
        "combine-windows",
        super::window_cmds::builtin_combine_windows,
        2,
        Some(2),
    );
    ctx.defsubr(
        "uncombine-window",
        super::window_cmds::builtin_uncombine_window,
        1,
        Some(1),
    );
    ctx.defsubr(
        "frame-windows-min-size",
        |_ctx, args| builtin_frame_windows_min_size(args),
        0,
        None,
    );
    ctx.defsubr(
        "remember-mouse-glyph",
        builtin_remember_mouse_glyph,
        0,
        None,
    );
    ctx.defsubr("obarrayp", |_ctx, args| builtin_obarrayp(args), 1, Some(1));
    ctx.defsubr("ntake", |_ctx, args| builtin_ntake(args), 2, Some(2));
    ctx.defsubr(
        "default-file-modes",
        |_ctx, args| super::fileio::builtin_default_file_modes(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "set-default-file-modes",
        |_ctx, args| super::fileio::builtin_set_default_file_modes(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "cancel-kbd-macro-events",
        builtin_cancel_kbd_macro_events,
        0,
        Some(0),
    );
    ctx.defsubr(
        "window-configuration-p",
        |_ctx, args| super::window_cmds::builtin_window_configuration_p(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "window-configuration-frame",
        |_ctx, args| super::window_cmds::builtin_window_configuration_frame(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "window-configuration-equal-p",
        |_ctx, args| super::window_cmds::builtin_window_configuration_equal_p(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "set-input-meta-mode",
        |_ctx, args| super::reader::builtin_set_input_meta_mode(args),
        1,
        Some(2),
    );
    ctx.defsubr(
        "set-output-flow-control",
        |_ctx, args| super::reader::builtin_set_output_flow_control(args),
        1,
        Some(2),
    );
    ctx.defsubr(
        "set-quit-char",
        super::reader::builtin_set_quit_char,
        1,
        Some(1),
    );
    ctx.defsubr_interactive(
        "top-level",
        |_ctx, args| super::minibuffer::builtin_top_level(args),
        0,
        Some(0),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    ctx.defsubr(
        "documentation-stringp",
        |_ctx, args| builtin_documentation_stringp(args),
        1,
        Some(1),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "internal--define-uninitialized-variable",
            symbols::builtin_internal_define_uninitialized_variable,
            1,
            Some(2),
        ),
    );
    ctx.defsubr(
        "compose-region-internal",
        super::composite::builtin_compose_region_internal,
        2,
        Some(4),
    );
    ctx.defsubr(
        "window-text-pixel-size",
        super::xdisp::builtin_window_text_pixel_size_ctx,
        0,
        Some(7),
    );
    ctx.defsubr(
        "pos-visible-in-window-p",
        super::xdisp::builtin_pos_visible_in_window_p_ctx,
        0,
        None,
    );
    ctx.defsubr(
        "frame--face-hash-table",
        super::xfaces::builtin_frame_face_hash_table,
        0,
        Some(1),
    );
    ctx.defsubr(
        "delete-directory-internal",
        super::fileio::builtin_delete_directory_internal,
        1,
        Some(1),
    );
    ctx.defsubr(
        "make-directory-internal",
        super::fileio::builtin_make_directory_internal,
        1,
        Some(1),
    );
    ctx.defsubr(
        "directory-files-and-attributes",
        super::dired::builtin_directory_files_and_attributes,
        1,
        Some(6),
    );
    ctx.defsubr(
        "find-file-name-handler",
        super::fileio::builtin_find_file_name_handler,
        2,
        Some(2),
    );
    ctx.defsubr(
        "file-name-all-completions",
        super::dired::builtin_file_name_all_completions,
        2,
        Some(2),
    );
    ctx.defsubr(
        "file-accessible-directory-p",
        super::fileio::builtin_file_accessible_directory_p,
        1,
        Some(1),
    );
    ctx.defsubr(
        "file-name-case-insensitive-p",
        super::fileio::builtin_file_name_case_insensitive_p,
        0,
        None,
    );
    ctx.defsubr(
        "file-newer-than-file-p",
        super::fileio::builtin_file_newer_than_file_p,
        2,
        Some(2),
    );
    ctx.defsubr(
        "verify-visited-file-modtime",
        super::fileio::builtin_verify_visited_file_modtime,
        0,
        Some(1),
    );
    ctx.defsubr(
        "internal-default-interrupt-process",
        super::process::builtin_internal_default_interrupt_process,
        0,
        Some(2),
    );
    ctx.defsubr(
        "internal-default-process-filter",
        super::process::builtin_internal_default_process_filter,
        0,
        None,
    );
    ctx.defsubr(
        "internal-default-process-sentinel",
        super::process::builtin_internal_default_process_sentinel,
        0,
        None,
    );
    ctx.defsubr(
        "internal-default-signal-process",
        super::process::builtin_internal_default_signal_process,
        0,
        None,
    );
    ctx.defsubr(
        "network-lookup-address-info",
        super::process::builtin_network_lookup_address_info,
        1,
        Some(3),
    );
    ctx.defsubr(
        "set-network-process-option",
        super::process::builtin_set_network_process_option,
        3,
        Some(4),
    );
    ctx.defsubr(
        "process-query-on-exit-flag",
        super::process::builtin_process_query_on_exit_flag,
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-process-query-on-exit-flag",
        super::process::builtin_set_process_query_on_exit_flag,
        2,
        Some(2),
    );
    ctx.defsubr(
        "process-inherit-coding-system-flag",
        super::process::builtin_process_inherit_coding_system_flag,
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-process-coding-system",
        super::process::builtin_set_process_coding_system,
        1,
        Some(3),
    );
    ctx.defsubr(
        "set-process-datagram-address",
        super::process::builtin_set_process_datagram_address,
        2,
        Some(2),
    );
    ctx.defsubr(
        "remove-list-of-text-properties",
        super::textprop::builtin_remove_list_of_text_properties,
        0,
        None,
    );
    ctx.defsubr(
        "get-char-property-and-overlay",
        super::textprop::builtin_get_char_property_and_overlay,
        2,
        Some(3),
    );
    ctx.defsubr(
        "next-single-property-change",
        super::textprop::builtin_next_single_property_change,
        2,
        Some(4),
    );
    ctx.defsubr(
        "previous-single-property-change",
        super::textprop::builtin_previous_single_property_change,
        0,
        None,
    );
    ctx.defsubr(
        "line-beginning-position",
        super::navigation::builtin_line_beginning_position,
        0,
        Some(1),
    );
    ctx.defsubr_interactive(
        "make-variable-buffer-local",
        super::custom::builtin_make_variable_buffer_local,
        1,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("vMake Variable Buffer Local: "),
    );
    ctx.defsubr(
        "active-minibuffer-window",
        super::window_cmds::builtin_active_minibuffer_window,
        0,
        None,
    );
    ctx.defsubr(
        "minibuffer-selected-window",
        super::window_cmds::builtin_minibuffer_selected_window,
        0,
        Some(0),
    );
    ctx.defsubr(
        "window-mode-line-height",
        super::window_cmds::builtin_window_mode_line_height,
        0,
        None,
    );
    ctx.defsubr(
        "window-header-line-height",
        super::window_cmds::builtin_window_header_line_height,
        0,
        None,
    );
    register_builtin(
        ctx,
        BuiltinRegistration::placeholder(
            "window-tab-line-height",
            super::window_cmds::builtin_window_tab_line_height,
            0,
            None,
            BuiltinNoEvalPlaceholder::FixnumZero,
        ),
    );
    ctx.defsubr(
        "set-window-display-table",
        super::window_cmds::builtin_set_window_display_table,
        2,
        Some(2),
    );
    ctx.defsubr(
        "set-window-cursor-type",
        super::window_cmds::builtin_set_window_cursor_type,
        0,
        None,
    );
    ctx.defsubr(
        "set-window-scroll-bars",
        super::window_cmds::builtin_set_window_scroll_bars,
        1,
        Some(6),
    );
    ctx.defsubr(
        "set-window-next-buffers",
        super::window_cmds::builtin_set_window_next_buffers,
        0,
        None,
    );
    ctx.defsubr(
        "set-window-prev-buffers",
        super::window_cmds::builtin_set_window_prev_buffers,
        0,
        None,
    );
    ctx.defsubr(
        "set-window-dedicated-p",
        super::window_cmds::builtin_set_window_dedicated_p,
        0,
        None,
    );
    ctx.defsubr(
        "delete-window-internal",
        super::window_cmds::builtin_delete_window_internal,
        1,
        Some(1),
    );
    ctx.defsubr_interactive(
        "delete-other-windows-internal",
        super::window_cmds::builtin_delete_other_windows_internal,
        0,
        Some(2),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    ctx.defsubr(
        "window-combination-limit",
        super::window_cmds::builtin_window_combination_limit,
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-window-combination-limit",
        super::window_cmds::builtin_set_window_combination_limit,
        2,
        Some(2),
    );
    ctx.defsubr(
        "window-resize-apply-total",
        super::window_cmds::builtin_window_resize_apply_total,
        0,
        Some(2),
    );
    ctx.defsubr(
        "other-window-for-scrolling",
        super::window_cmds::builtin_other_window_for_scrolling,
        0,
        Some(0),
    );
    // `select-frame-set-input-focus' is lisp/frame.el:1262, not a DEFUN
    // (DIVERGENCES.md 154).  Its body is `select-frame' + `x-focus-frame' +
    // `raise-frame', all three C DEFUNs that stay registered.
    ctx.defsubr(
        "modify-frame-parameters",
        super::frame::builtin_modify_frame_parameters,
        2,
        Some(2),
    );
    ctx.defsubr(
        "frame-selected-window",
        super::window_cmds::builtin_frame_selected_window,
        0,
        None,
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "frame-old-selected-window",
            super::window_cmds::builtin_frame_old_selected_window,
            0,
            None,
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "set-frame-selected-window",
            super::window_cmds::builtin_set_frame_selected_window,
            2,
            Some(3),
        ),
    );
    ctx.defsubr(
        "x-display-pixel-width",
        super::display::builtin_x_display_pixel_width,
        0,
        Some(1),
    );
    ctx.defsubr(
        "x-display-pixel-height",
        super::display::builtin_x_display_pixel_height,
        0,
        Some(1),
    );
    ctx.defsubr(
        "x-server-max-request-size",
        super::display::builtin_x_server_max_request_size,
        0,
        Some(1),
    );
    ctx.defsubr(
        "x-display-grayscale-p",
        super::display::builtin_x_display_grayscale_p,
        0,
        Some(1),
    );
    ctx.defsubr(
        "x-display-backing-store",
        super::display::builtin_x_display_backing_store,
        0,
        Some(1),
    );
    ctx.defsubr(
        "x-display-color-cells",
        super::display::builtin_x_display_color_cells,
        0,
        Some(1),
    );
    ctx.defsubr(
        "x-display-save-under",
        super::display::builtin_x_display_save_under,
        0,
        Some(1),
    );
    ctx.defsubr(
        "x-display-set-last-user-time",
        super::display::builtin_x_display_set_last_user_time,
        0,
        None,
    );
    ctx.defsubr(
        "x-display-visual-class",
        super::display::builtin_x_display_visual_class,
        0,
        Some(1),
    );
    ctx.defsubr(
        "minor-mode-key-binding",
        super::interactive::builtin_minor_mode_key_binding,
        1,
        Some(2),
    );
    ctx.defsubr(
        "this-command-keys-vector",
        super::interactive::builtin_this_command_keys_vector,
        0,
        Some(0),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::placeholder(
            "this-single-command-keys",
            super::interactive::builtin_this_single_command_keys,
            0,
            Some(0),
            BuiltinNoEvalPlaceholder::Nil,
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::placeholder(
            "this-single-command-raw-keys",
            super::interactive::builtin_this_single_command_raw_keys,
            0,
            Some(0),
            BuiltinNoEvalPlaceholder::Nil,
        ),
    );
    ctx.defsubr(
        "clear-this-command-keys",
        super::interactive::builtin_clear_this_command_keys,
        0,
        Some(1),
    );
    ctx.defsubr(
        "waiting-for-user-input-p",
        super::reader::builtin_waiting_for_user_input_p_ctx,
        0,
        Some(0),
    );
    ctx.defsubr(
        "minibuffer-prompt",
        super::minibuffer::builtin_minibuffer_prompt_ctx,
        0,
        Some(0),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "minibuffer-prompt-end",
            super::minibuffer::builtin_minibuffer_prompt_end_ctx,
            0,
            Some(0),
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "innermost-minibuffer-p",
            super::minibuffer::builtin_innermost_minibuffer_p_ctx,
            0,
            None,
        ),
    );
    ctx.defsubr(
        "backtrace--frames-from-thread",
        super::misc::builtin_backtrace_frames_from_thread,
        1,
        Some(1),
    );
    ctx.defsubr_interactive(
        "abort-minibuffers",
        super::minibuffer::builtin_abort_minibuffers_ctx,
        0,
        Some(0),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    ctx.defsubr(
        "set-marker-insertion-type",
        super::marker::builtin_set_marker_insertion_type,
        0,
        None,
    );
    ctx.defsubr(
        "set-standard-case-table",
        super::casetab::builtin_set_standard_case_table,
        0,
        None,
    );
    ctx.defsubr(
        "get-unused-category",
        super::category::builtin_get_unused_category,
        0,
        Some(1),
    );
    ctx.defsubr(
        "standard-category-table",
        super::category::builtin_standard_category_table,
        0,
        None,
    );
    ctx.defsubr_interactive(
        "upcase-initials-region",
        super::casefiddle::builtin_upcase_initials_region,
        2,
        Some(3),
        super::interactive::BuiltinInteractiveSpec::Form(region_noncontiguous_interactive_spec),
    );
    ctx.defsubr(
        "buffer-substring-no-properties",
        |eval, args| super::editfns::builtin_buffer_substring_no_properties(eval, args),
        0,
        None,
    );

    // Pure builtins from builtins_extra (previously in old match dispatch).
    // These don't need &mut Context, so we wrap them.
    macro_rules! defsubr_pure {
        ($ctx:expr, $name:expr, $func:expr) => {
            $ctx.defsubr($name, |_eval, args| $func(args), 0, None);
        };
    }
    defsubr_pure!(ctx, "take", super::builtins_extra::builtin_take);
    defsubr_pure!(
        ctx,
        "assoc-string",
        super::builtins_extra::builtin_assoc_string
    );
    defsubr_pure!(
        ctx,
        "string-search",
        super::builtins_extra::builtin_string_search
    );
    ctx.defsubr_1(
        "bare-symbol",
        super::builtins_extra::builtin_bare_symbol_1,
        1,
    );
    defsubr_pure!(
        ctx,
        "bare-symbol-p",
        super::builtins_extra::builtin_bare_symbol_p
    );
    defsubr_pure!(ctx, "byteorder", super::builtins_extra::builtin_byteorder);
    defsubr_pure!(
        ctx,
        "car-less-than-car",
        super::builtins_extra::builtin_car_less_than_car
    );
    defsubr_pure!(
        ctx,
        "proper-list-p",
        super::builtins_extra::builtin_proper_list_p
    );
    defsubr_pure!(ctx, "subrp", super::builtins_extra::builtin_subrp);
    defsubr_pure!(
        ctx,
        "byte-code-function-p",
        super::builtins_extra::builtin_byte_code_function_p
    );
    ctx.defsubr_1("closurep", super::builtins_extra::builtin_closurep_1, 1);
    defsubr_pure!(ctx, "natnump", super::builtins_extra::builtin_natnump);
    // GNU defines `fixnump` and `bignump` in `lisp/subr.el` (not in C),
    // so they must come from the loaded Lisp source — registering Rust
    // subrs here would shadow the elisp definitions and make
    // `(subrp (symbol-function 'fixnump))` return t instead of nil.
    ctx.defsubr(
        "user-login-name",
        super::builtins_extra::builtin_user_login_name,
        0,
        Some(1),
    );
    ctx.defsubr(
        "user-real-login-name",
        super::builtins_extra::builtin_user_real_login_name,
        0,
        Some(0),
    );
    ctx.defsubr(
        "user-full-name",
        super::builtins_extra::builtin_user_full_name,
        0,
        Some(1),
    );
    ctx.defsubr(
        "system-name",
        super::builtins_extra::builtin_system_name,
        0,
        Some(0),
    );
    defsubr_pure!(ctx, "emacs-pid", super::builtins_extra::builtin_emacs_pid);
    defsubr_pure!(
        ctx,
        "memory-use-counts",
        super::builtins_extra::builtin_memory_use_counts
    );
    defsubr_pure!(
        ctx,
        "neomacs--heap-layout-stats",
        super::builtins_extra::builtin_neomacs_heap_layout_stats
    );

    // -----------------------------------------------------------------------
    // Additional builtins registered via defsubr.
    // -----------------------------------------------------------------------

    // -- Arithmetic --
    ctx.defsubr_slice("+", super::builtins::arithmetic::builtin_add_slice, 0, None);
    ctx.defsubr_slice("-", super::builtins::arithmetic::builtin_sub_slice, 0, None);
    ctx.defsubr("*", |_ctx, args| builtin_mul(args), 0, None);
    ctx.defsubr("/", |_ctx, args| builtin_div(args), 1, None);
    ctx.defsubr_2("%", builtin_percent, 2);
    ctx.defsubr_2("mod", builtin_mod, 2);
    ctx.defsubr_1("1+", builtin_add1_1, 1);
    ctx.defsubr_1("1-", builtin_sub1_1, 1);
    ctx.defsubr_slice("max", builtin_max_slice, 1, None);
    ctx.defsubr_slice("min", builtin_min_slice, 1, None);
    ctx.defsubr("abs", |_ctx, args| builtin_abs(args), 1, Some(1));

    // -- Logical / bitwise --
    ctx.defsubr_slice("logand", |_ctx, args| builtin_logand_slice(args), 0, None);
    ctx.defsubr_slice("logior", |_ctx, args| builtin_logior_slice(args), 0, None);
    ctx.defsubr_slice("logxor", |_ctx, args| builtin_logxor_slice(args), 0, None);
    ctx.defsubr("lognot", |_ctx, args| builtin_lognot(args), 1, Some(1));
    ctx.defsubr_slice("ash", |_ctx, args| builtin_ash_slice(args), 2, Some(2));

    // -- Numeric comparisons --
    ctx.defsubr_slice("=", builtin_num_eq_slice, 1, None);
    ctx.defsubr_slice("<", builtin_num_lt_slice, 1, None);
    ctx.defsubr_slice("<=", builtin_num_le_slice, 1, None);
    ctx.defsubr_slice(">", builtin_num_gt_slice, 1, None);
    ctx.defsubr_slice(">=", builtin_num_ge_slice, 1, None);
    ctx.defsubr_2("/=", builtin_num_ne_2, 2);

    // -- Type predicates --
    ctx.defsubr_1("null", builtin_null_1, 1);
    // No `not': GNU has no DEFUN of that name.  `(defalias 'not #'null)'
    // (lisp/subr.el:71) puts the SYMBOL `null' in the cell, and a compiled
    // caller emits the Bnot opcode instead -- DIVERGENCES.md 148.
    ctx.defsubr_1("atom", builtin_atom_1, 1);
    ctx.defsubr_1("consp", builtin_consp_1, 1);
    ctx.defsubr_1("listp", builtin_listp_1, 1);
    ctx.defsubr_1("nlistp", builtin_nlistp_1, 1);
    ctx.defsubr_1("symbolp", builtin_symbolp_1, 1);
    ctx.defsubr_1("numberp", builtin_numberp_1, 1);
    ctx.defsubr_1("integerp", builtin_integerp_1, 1);
    ctx.defsubr_1("floatp", builtin_floatp_1, 1);
    ctx.defsubr_1("stringp", builtin_stringp_1, 1);
    ctx.defsubr_1("vectorp", builtin_vectorp_1, 1);
    ctx.defsubr(
        "characterp",
        |_ctx, args| builtin_characterp(args),
        1,
        Some(2),
    );
    // No `booleanp', `integer-or-null-p', `string-or-null-p',
    // `list-of-strings-p' or `char-uppercase-p': GNU has no DEFUN for any of
    // the six type predicates.  They are `defun's at lisp/subr.el:4762-4812
    // and lisp/simple.el:6683 over primitives that ARE in C
    // (`stringp', `integerp', `indirect-function',
    // `get-char-code-property') -- DIVERGENCES.md 148.
    ctx.defsubr_1("keywordp", builtin_keywordp_1, 1);
    ctx.defsubr(
        "hash-table-p",
        |_ctx, args| builtin_hash_table_p(args),
        1,
        Some(1),
    );
    ctx.defsubr("bufferp", |_ctx, args| builtin_bufferp(args), 1, Some(1));
    ctx.defsubr(
        "type-of",
        super::builtins::types::builtin_type_of_with_ctx,
        1,
        Some(1),
    );
    ctx.defsubr(
        "sequencep",
        |_ctx, args| builtin_sequencep(args),
        1,
        Some(1),
    );
    ctx.defsubr("arrayp", |_ctx, args| builtin_arrayp(args), 1, Some(1));
    // `ignore' is not here: it is a `defun' at lisp/subr.el:501, and the
    // byte compiler names it itself -- `(byte-defop-compiler-1 ignore)',
    // lisp/emacs-lisp/bytecomp.el:4429 -- so a compiled `(ignore X)' emits
    // Bconstant nil and never reads the cell (DIVERGENCES.md 152).
    ctx.defsubr(
        "cl-type-of",
        |_ctx, args| builtin_cl_type_of(args),
        1,
        Some(1),
    );

    // -- Equality --
    ctx.defsubr_2("eq", builtin_eq_2, 2);
    ctx.defsubr_2("eql", builtin_eql_2, 2);
    ctx.defsubr_2("equal", builtin_equal_2, 2);

    // -- Cons / List --
    ctx.defsubr_2("cons", builtin_cons_2, 2);
    ctx.defsubr_1("car", builtin_car_1, 1);
    ctx.defsubr_1("cdr", builtin_cdr_1, 1);
    ctx.defsubr_1("car-safe", builtin_car_safe_1, 1);
    ctx.defsubr_1("cdr-safe", builtin_cdr_safe_1, 1);
    ctx.defsubr_2("setcar", builtin_setcar_2, 2);
    ctx.defsubr_2("setcdr", builtin_setcdr_2, 2);
    ctx.defsubr_slice("list", builtin_list_slice, 0, None);
    ctx.defsubr_1("length", builtin_length_1, 1);
    ctx.defsubr_2("nth", builtin_nth_2, 2);
    ctx.defsubr_2("nthcdr", builtin_nthcdr_2, 2);
    ctx.defsubr_slice("append", builtin_append_slice, 0, None);
    ctx.defsubr("reverse", |_ctx, args| builtin_reverse(args), 1, Some(1));
    ctx.defsubr_1("nreverse", builtin_nreverse_1, 1);
    ctx.defsubr_2("member", builtin_member_2, 2);
    ctx.defsubr_2("memq", builtin_memq_2, 2);
    ctx.defsubr_2("assq", builtin_assq_2, 2);
    ctx.defsubr(
        "copy-sequence",
        |_ctx, args| builtin_copy_sequence(args),
        1,
        Some(1),
    );
    ctx.defsubr_slice("plist-get", builtin_plist_get_slice, 2, Some(3));
    ctx.defsubr("plist-put", builtin_plist_put_with_ctx, 3, Some(4));
    ctx.defsubr(
        "copy-alist",
        |_ctx, args| super::misc::builtin_copy_alist(args),
        1,
        Some(1),
    );
    ctx.defsubr("rassoc", super::misc::builtin_rassoc_with_ctx, 2, Some(2));
    ctx.defsubr_2("rassq", super::misc::builtin_rassq_2, 2);
    ctx.defsubr(
        "make-list",
        |_ctx, args| super::misc::builtin_make_list(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "safe-length",
        |_ctx, args| super::misc::builtin_safe_length(args),
        1,
        Some(1),
    );

    // -- String --
    // GNU DEFUNs `string-equal' and `string-lessp' (src/fns.c) and nothing
    // else here: `string=', `string<' and `string>' are `defalias'es at
    // lisp/subr.el:2277-2279, so their cells hold the TARGET SYMBOL, and a
    // compiled caller emits Bstringeqlsign / Bstringlss instead
    // (DIVERGENCES.md 148).  `string-greaterp' is itself Lisp
    // (lisp/subr.el:6283) with a `compiler-macro' (:6287-6290) that swaps
    // its arguments into `string-lessp', so it is not registered either
    // (DIVERGENCES.md 152).
    ctx.defsubr_2("string-equal", builtin_string_equal_2, 2);
    ctx.defsubr_2("string-lessp", builtin_string_lessp_2, 2);
    ctx.defsubr_slice(
        "substring",
        |_ctx, args| builtin_substring_slice(args),
        1,
        Some(3),
    );
    ctx.defsubr_slice("concat", |_ctx, args| builtin_concat_slice(args), 0, None);
    ctx.defsubr(
        "unibyte-string",
        |_ctx, args| builtin_unibyte_string(args),
        0,
        None,
    );
    ctx.defsubr_2("string-to-number", builtin_string_to_number, 1);
    ctx.defsubr(
        "number-to-string",
        |ctx, args| builtin_number_to_string(ctx, args),
        1,
        Some(1),
    );
    ctx.defsubr("upcase", builtin_upcase_in_state, 1, Some(1));
    ctx.defsubr("downcase", builtin_downcase_in_state, 1, Some(1));
    ctx.defsubr(
        "char-to-string",
        |_ctx, args| builtin_char_to_string(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "string-to-char",
        |_ctx, args| builtin_string_to_char(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "clear-string",
        |_ctx, args| builtin_clear_string(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "compare-strings",
        |_ctx, args| super::fns::builtin_compare_strings(args),
        6,
        Some(7),
    );
    ctx.defsubr(
        "string-version-lessp",
        super::fns::builtin_string_version_lessp,
        2,
        Some(2),
    );
    ctx.defsubr(
        "string-collate-lessp",
        super::fns::builtin_string_collate_lessp,
        2,
        Some(4),
    );
    ctx.defsubr(
        "string-collate-equalp",
        super::fns::builtin_string_collate_equalp,
        2,
        Some(4),
    );
    ctx.defsubr_2(
        "equal-including-properties",
        super::fns::builtin_equal_including_properties_2,
        2,
    );
    ctx.defsubr(
        "string-make-multibyte",
        |_ctx, args| super::fns::builtin_string_make_multibyte(args),
        0,
        None,
    );
    ctx.defsubr(
        "string-make-unibyte",
        |_ctx, args| super::fns::builtin_string_make_unibyte(args),
        0,
        None,
    );
    ctx.defsubr(
        "string-to-multibyte",
        |_ctx, args| super::misc::builtin_string_to_multibyte(args),
        0,
        None,
    );
    ctx.defsubr(
        "string-to-unibyte",
        |_ctx, args| super::misc::builtin_string_to_unibyte(args),
        0,
        None,
    );
    ctx.defsubr(
        "string-as-unibyte",
        |_ctx, args| super::misc::builtin_string_as_unibyte(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "string-as-multibyte",
        |_ctx, args| super::misc::builtin_string_as_multibyte(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "unibyte-char-to-multibyte",
        |_ctx, args| super::misc::builtin_unibyte_char_to_multibyte(args),
        0,
        None,
    );
    ctx.defsubr(
        "multibyte-char-to-unibyte",
        |_ctx, args| super::misc::builtin_multibyte_char_to_unibyte(args),
        0,
        None,
    );

    // -- Vector --
    ctx.defsubr(
        "make-vector",
        |_ctx, args| builtin_make_vector(args),
        2,
        Some(2),
    );
    ctx.defsubr_slice("vector", builtin_vector_slice, 0, None);
    ctx.defsubr_2("aref", builtin_aref_2, 2);
    ctx.defsubr("aset", |_ctx, args| builtin_aset(args), 3, Some(3));
    ctx.defsubr_slice("vconcat", |_ctx, args| builtin_vconcat_slice(args), 0, None);

    // -- Hash table --
    ctx.defsubr_slice(
        "make-hash-table",
        |_ctx, args| builtin_make_hash_table_slice(args),
        0,
        None,
    );
    ctx.defsubr_3("gethash", builtin_gethash_3, 2);
    ctx.defsubr_3("puthash", builtin_puthash_3, 3);
    ctx.defsubr_2("remhash", builtin_remhash_2, 2);
    ctx.defsubr("clrhash", |_ctx, args| builtin_clrhash(args), 1, Some(1));
    ctx.defsubr(
        "hash-table-count",
        |_ctx, args| builtin_hash_table_count(args),
        1,
        Some(1),
    );

    // -- Float / math / conversion --
    ctx.defsubr("float", |_ctx, args| builtin_float(args), 1, Some(1));
    ctx.defsubr("truncate", |_ctx, args| builtin_truncate(args), 1, Some(2));
    ctx.defsubr("floor", |_ctx, args| builtin_floor(args), 1, Some(2));
    ctx.defsubr("ceiling", |_ctx, args| builtin_ceiling(args), 1, Some(2));
    ctx.defsubr("round", |_ctx, args| builtin_round(args), 1, Some(2));
    ctx.defsubr(
        "copysign",
        |_ctx, args| super::floatfns::builtin_copysign(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "frexp",
        |_ctx, args| super::floatfns::builtin_frexp(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "ldexp",
        |_ctx, args| super::floatfns::builtin_ldexp(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "logb",
        |_ctx, args| super::floatfns::builtin_logb(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "fceiling",
        |_ctx, args| super::floatfns::builtin_fceiling(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "ffloor",
        |_ctx, args| super::floatfns::builtin_ffloor(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "fround",
        |_ctx, args| super::floatfns::builtin_fround(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "ftruncate",
        |_ctx, args| super::floatfns::builtin_ftruncate(args),
        1,
        Some(1),
    );

    // -- Symbol --
    ctx.defsubr_1("symbol-name", builtin_symbol_name_1, 1);
    ctx.defsubr_1("make-symbol", builtin_make_symbol_1, 1);

    // -- Misc pure --
    ctx.defsubr(
        "bitmap-spec-p",
        |_ctx, args| builtin_bitmap_spec_p(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "byte-to-string",
        |_ctx, args| builtin_byte_to_string(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "clear-buffer-auto-save-failure",
        |_ctx, args| builtin_clear_buffer_auto_save_failure(args),
        0,
        None,
    );
    ctx.defsubr("clear-face-cache", builtin_clear_face_cache, 0, Some(1));
    ctx.defsubr(
        "combine-after-change-execute",
        builtin_combine_after_change_execute,
        0,
        Some(0),
    );
    ctx.defsubr(
        "command-error-default-function",
        builtin_command_error_default_function,
        3,
        Some(3),
    );
    ctx.defsubr(
        "locale-info",
        |_ctx, args| super::misc::builtin_locale_info(args),
        1,
        Some(1),
    );
    ctx.defsubr_slice("nconc", builtin_nconc_slice, 0, None);

    // -- Subr introspection --
    ctx.defsubr(
        "subr-name",
        |_ctx, args| super::subr_info::builtin_subr_name(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "subr-arity",
        super::subr_info::builtin_subr_arity,
        1,
        Some(1),
    );
    ctx.defsubr(
        "native-comp-function-p",
        |_ctx, args| super::subr_info::builtin_native_comp_function_p(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "interpreted-function-p",
        |_ctx, args| super::subr_info::builtin_interpreted_function_p(args),
        0,
        None,
    );
    ctx.defsubr("func-arity", builtin_func_arity, 1, Some(1));

    // -- Character encoding --
    ctx.defsubr(
        "char-width",
        |ctx, args| crate::encoding::builtin_char_width_in_context(ctx, args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "string-bytes",
        |_ctx, args| crate::encoding::builtin_string_bytes(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "multibyte-string-p",
        |_ctx, args| crate::encoding::builtin_multibyte_string_p(args),
        0,
        None,
    );
    ctx.defsubr(
        "encode-coding-string",
        crate::encoding::builtin_encode_coding_string_in_context,
        2,
        Some(4),
    );
    ctx.defsubr(
        "decode-coding-string",
        crate::encoding::builtin_decode_coding_string_in_context,
        2,
        Some(4),
    );
    ctx.defsubr(
        "char-or-string-p",
        |_ctx, args| crate::encoding::builtin_char_or_string_p(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "max-char",
        |_ctx, args| crate::encoding::builtin_max_char(args),
        0,
        Some(1),
    );

    // -- Search --
    ctx.defsubr(
        "regexp-quote",
        |_ctx, args| super::search::builtin_regexp_quote(args),
        1,
        Some(1),
    );

    // -- File I/O --
    ctx.defsubr(
        "file-attributes-lessp",
        super::dired::builtin_file_attributes_lessp,
        2,
        Some(2),
    );
    ctx.defsubr(
        "system-users",
        |_ctx, args| super::dired::builtin_system_users(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "system-groups",
        |_ctx, args| super::dired::builtin_system_groups(args),
        0,
        Some(0),
    );

    // -- User / editfns --
    ctx.defsubr(
        "user-uid",
        |_ctx, args| super::editfns::builtin_user_uid(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "user-real-uid",
        |_ctx, args| super::editfns::builtin_user_real_uid(args),
        0,
        Some(0),
    );

    // -- Time/date --
    ctx.defsubr(
        "time-add",
        |_ctx, args| super::timefns::builtin_time_add(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "time-subtract",
        |_ctx, args| super::timefns::builtin_time_subtract(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "time-less-p",
        |_ctx, args| super::timefns::builtin_time_less_p(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "time-equal-p",
        |_ctx, args| super::timefns::builtin_time_equal_p(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "current-time-string",
        |_ctx, args| super::timefns::builtin_current_time_string(args),
        0,
        Some(2),
    );
    ctx.defsubr(
        "current-time-zone",
        |_ctx, args| super::timefns::builtin_current_time_zone(args),
        0,
        Some(2),
    );
    ctx.defsubr(
        "encode-time",
        |_ctx, args| super::timefns::builtin_encode_time(args),
        1,
        None,
    );
    ctx.defsubr(
        "decode-time",
        |_ctx, args| super::timefns::builtin_decode_time(args),
        0,
        Some(3),
    );
    ctx.defsubr(
        "time-convert",
        super::timefns::builtin_time_convert_in_context,
        1,
        Some(2),
    );
    ctx.defsubr(
        "set-time-zone-rule",
        |_ctx, args| super::timefns::builtin_set_time_zone_rule(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "format-time-string",
        |_ctx, args| super::format::builtin_format_time_string(args),
        1,
        Some(3),
    );

    // -- Case/char --
    ctx.defsubr(
        "upcase-initials",
        super::casefiddle::builtin_upcase_initials_in_state,
        1,
        Some(1),
    );
    ctx.defsubr(
        "char-resolve-modifiers",
        |_ctx, args| super::casefiddle::builtin_char_resolve_modifiers(args),
        0,
        None,
    );

    // -- Font/face --
    ctx.defsubr(
        "fontp",
        |_ctx, args| super::font::builtin_fontp(args),
        1,
        Some(2),
    );
    ctx.defsubr(
        "font-spec",
        |_ctx, args| super::font::builtin_font_spec(args),
        0,
        None,
    );
    ctx.defsubr(
        "font-get",
        |_ctx, args| super::font::builtin_font_get(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "font-put",
        |_ctx, args| super::font::builtin_font_put(args),
        3,
        Some(3),
    );
    ctx.defsubr(
        "font-xlfd-name",
        |_ctx, args| super::font::builtin_font_xlfd_name(args),
        1,
        Some(3),
    );
    ctx.defsubr(
        "close-font",
        |_ctx, args| super::font::builtin_close_font(args),
        1,
        Some(2),
    );
    ctx.defsubr(
        "clear-font-cache",
        |_ctx, args| super::font::builtin_clear_font_cache(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "internal-lisp-face-attribute-values",
        |_ctx, args| super::xfaces::builtin_internal_lisp_face_attribute_values(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "internal-lisp-face-equal-p",
        super::xfaces::builtin_internal_lisp_face_equal_p,
        0,
        None,
    );
    ctx.defsubr(
        "internal-lisp-face-empty-p",
        super::xfaces::builtin_internal_lisp_face_empty_p,
        0,
        None,
    );
    ctx.defsubr(
        "face-attribute-relative-p",
        |_ctx, args| super::xfaces::builtin_face_attribute_relative_p(args),
        0,
        None,
    );
    ctx.defsubr(
        "merge-face-attribute",
        super::xfaces::builtin_merge_face_attribute_with_eval,
        3,
        Some(3),
    );
    ctx.defsubr(
        "color-gray-p",
        super::xfaces::builtin_color_gray_p,
        1,
        Some(2),
    );
    ctx.defsubr(
        "color-supported-p",
        |_ctx, args| super::xfaces::builtin_color_supported_p(args),
        1,
        Some(3),
    );
    ctx.defsubr(
        "color-distance",
        super::xfaces::builtin_color_distance,
        2,
        Some(4),
    );
    ctx.defsubr(
        "color-values-from-color-spec",
        |_ctx, args| super::xfaces::builtin_color_values_from_color_spec(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "internal-face-x-get-resource",
        |_ctx, args| super::xfaces::builtin_internal_face_x_get_resource(args),
        2,
        Some(3),
    );
    ctx.defsubr(
        "internal-set-font-selection-order",
        |_ctx, args| super::xfaces::builtin_internal_set_font_selection_order(args),
        0,
        None,
    );
    ctx.defsubr(
        "internal-set-alternative-font-family-alist",
        |_ctx, args| super::xfaces::builtin_internal_set_alternative_font_family_alist(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "internal-set-alternative-font-registry-alist",
        |_ctx, args| super::xfaces::builtin_internal_set_alternative_font_registry_alist(args),
        0,
        None,
    );
    ctx.defsubr(
        "internal-copy-lisp-face",
        super::xfaces::builtin_internal_copy_lisp_face,
        4,
        Some(4),
    );
    ctx.defsubr(
        "internal-get-lisp-face-attribute",
        super::xfaces::builtin_internal_get_lisp_face_attribute,
        2,
        Some(3),
    );
    ctx.defsubr(
        "internal-merge-in-global-face",
        super::xfaces::builtin_internal_merge_in_global_face,
        0,
        None,
    );

    // -- Case table --
    ctx.defsubr(
        "case-table-p",
        |_ctx, args| super::casetab::builtin_case_table_p(args),
        1,
        Some(1),
    );

    // -- Category --
    ctx.defsubr(
        "category-table-p",
        |_ctx, args| super::category::builtin_category_table_p(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "copy-category-table",
        |_ctx, args| super::category::builtin_copy_category_table(args),
        0,
        Some(1),
    );
    ctx.defsubr(
        "make-category-table",
        |_ctx, args| super::category::builtin_make_category_table(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "category-set-mnemonics",
        |_ctx, args| super::category::builtin_category_set_mnemonics(args),
        0,
        None,
    );

    // -- Char-table / bool-vector --
    ctx.defsubr(
        "char-table-p",
        |_ctx, args| super::chartable::builtin_char_table_p(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-char-table-range",
        |ctx, args| super::chartable::builtin_set_char_table_range(args, Some(&ctx.obarray)),
        3,
        Some(3),
    );
    ctx.defsubr(
        "char-table-range",
        |ctx, args| super::chartable::builtin_char_table_range(args, Some(&ctx.obarray)),
        2,
        Some(2),
    );
    ctx.defsubr(
        "char-table-parent",
        |_ctx, args| super::chartable::builtin_char_table_parent(args),
        0,
        None,
    );
    ctx.defsubr(
        "set-char-table-parent",
        |_ctx, args| super::chartable::builtin_set_char_table_parent(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "char-table-extra-slot",
        |_ctx, args| super::chartable::builtin_char_table_extra_slot(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "set-char-table-extra-slot",
        |_ctx, args| super::chartable::builtin_set_char_table_extra_slot(args),
        3,
        Some(3),
    );
    ctx.defsubr(
        "char-table-subtype",
        |_ctx, args| super::chartable::builtin_char_table_subtype(args),
        0,
        None,
    );
    ctx.defsubr(
        "bool-vector",
        |_ctx, args| super::chartable::builtin_bool_vector(args),
        0,
        None,
    );
    ctx.defsubr(
        "make-bool-vector",
        |_ctx, args| super::chartable::builtin_make_bool_vector(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "bool-vector-count-population",
        |_ctx, args| super::chartable::builtin_bool_vector_count_population(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "bool-vector-count-consecutive",
        |_ctx, args| super::chartable::builtin_bool_vector_count_consecutive(args),
        3,
        Some(3),
    );
    ctx.defsubr(
        "bool-vector-intersection",
        |_ctx, args| super::chartable::builtin_bool_vector_intersection(args),
        2,
        Some(3),
    );
    ctx.defsubr(
        "bool-vector-not",
        |_ctx, args| super::chartable::builtin_bool_vector_not(args),
        1,
        Some(2),
    );
    ctx.defsubr(
        "bool-vector-set-difference",
        |_ctx, args| super::chartable::builtin_bool_vector_set_difference(args),
        0,
        None,
    );
    ctx.defsubr(
        "bool-vector-union",
        |_ctx, args| super::chartable::builtin_bool_vector_union(args),
        0,
        None,
    );
    ctx.defsubr(
        "bool-vector-exclusive-or",
        |_ctx, args| super::chartable::builtin_bool_vector_exclusive_or(args),
        2,
        Some(3),
    );
    ctx.defsubr(
        "bool-vector-subsetp",
        |_ctx, args| super::chartable::builtin_bool_vector_subsetp(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "make-char-table",
        super::chartable::builtin_make_char_table,
        1,
        Some(2),
    );

    // -- Charset --
    ctx.defsubr(
        "charset-priority-list",
        |_ctx, args| super::charset::builtin_charset_priority_list(args),
        0,
        None,
    );
    ctx.defsubr(
        "set-charset-priority",
        |_ctx, args| super::charset::builtin_set_charset_priority(args),
        1,
        None,
    );
    ctx.defsubr(
        "char-charset",
        |_ctx, args| super::charset::builtin_char_charset(args),
        1,
        Some(2),
    );
    ctx.defsubr(
        "charset-id-internal",
        |_ctx, args| super::charset::builtin_charset_id_internal(args),
        0,
        None,
    );
    ctx.defsubr(
        "declare-equiv-charset",
        |_ctx, args| super::charset::builtin_declare_equiv_charset(args),
        4,
        Some(4),
    );
    ctx.defsubr(
        "find-charset-string",
        |_ctx, args| super::charset::builtin_find_charset_string(args),
        1,
        Some(2),
    );
    ctx.defsubr(
        "decode-big5-char",
        |_ctx, args| super::charset::builtin_decode_big5_char(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "decode-char",
        |_ctx, args| super::charset::builtin_decode_char(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "decode-sjis-char",
        |_ctx, args| super::charset::builtin_decode_sjis_char(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "encode-big5-char",
        |_ctx, args| super::charset::builtin_encode_big5_char(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "encode-char",
        |_ctx, args| super::charset::builtin_encode_char(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "encode-sjis-char",
        |_ctx, args| super::charset::builtin_encode_sjis_char(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "get-unused-iso-final-char",
        |_ctx, args| super::charset::builtin_get_unused_iso_final_char(args),
        0,
        None,
    );
    ctx.defsubr(
        "clear-charset-maps",
        |_ctx, args| super::charset::builtin_clear_charset_maps(args),
        0,
        None,
    );

    // -- Coding system (eval-dependent via coding_systems field) --
    ctx.defsubr("coding-system-p", defsubr_coding_system_p, 1, Some(1));
    ctx.defsubr("check-coding-system", defsubr_check_coding_system, 0, None);
    ctx.defsubr(
        "check-coding-systems-region",
        defsubr_check_coding_systems_region,
        3,
        Some(3),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "define-coding-system-internal",
            defsubr_define_coding_system_internal,
            13,
            None,
        ),
    );
    ctx.defsubr(
        "define-coding-system-alias",
        defsubr_define_coding_system_alias,
        2,
        Some(2),
    );
    ctx.defsubr(
        "set-coding-system-priority",
        defsubr_set_coding_system_priority,
        0,
        None,
    );
    ctx.defsubr(
        "set-keyboard-coding-system-internal",
        defsubr_set_keyboard_coding_system_internal,
        1,
        Some(2),
    );
    ctx.defsubr(
        "set-safe-terminal-coding-system-internal",
        defsubr_set_safe_terminal_coding_system_internal,
        1,
        Some(1),
    );
    ctx.defsubr(
        "set-terminal-coding-system-internal",
        defsubr_set_terminal_coding_system_internal,
        1,
        Some(2),
    );
    ctx.defsubr(
        "set-text-conversion-style",
        |_ctx, args| super::coding::builtin_set_text_conversion_style(args),
        0,
        None,
    );
    ctx.defsubr(
        "text-quoting-style",
        |ctx, args| super::coding::builtin_text_quoting_style(ctx, args),
        0,
        Some(0),
    );
    // `set-buffer-file-coding-system' is not here: it is a `defun' at
    // lisp/international/mule.el:1302 that merges coding systems, sets
    // `buffer-file-coding-system-explicit' and marks the buffer modified
    // (DIVERGENCES.md 152).

    // -- CCL (eval-dependent) --
    ctx.defsubr("ccl-program-p", builtin_ccl_program_p, 1, Some(1));
    ctx.defsubr("ccl-execute", builtin_ccl_execute, 2, Some(2));
    ctx.defsubr(
        "ccl-execute-on-string",
        builtin_ccl_execute_on_string,
        3,
        Some(5),
    );
    ctx.defsubr(
        "register-ccl-program",
        builtin_register_ccl_program,
        0,
        None,
    );
    ctx.defsubr(
        "register-code-conversion-map",
        builtin_register_code_conversion_map,
        0,
        None,
    );

    // -- Eval builtins (eval-dependent) --
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("defconst-1", builtin_defconst_1, 2, Some(3)),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state("defvar-1", builtin_defvar_1, 2, Some(3)),
    );
    ctx.defsubr(
        "yes-or-no-p",
        super::reader::builtin_yes_or_no_p,
        1,
        Some(1),
    );
    ctx.defsubr(
        "locate-file-internal",
        super::lread::builtin_locate_file_internal,
        2,
        Some(4),
    );

    // -- Dispnew --
    ctx.defsubr_interactive(
        "redraw-display",
        |_ctx, args| super::dispnew::pure::builtin_redraw_display(args),
        0,
        Some(0),
        super::interactive::BuiltinInteractiveSpec::String(""),
    );
    ctx.defsubr_interactive(
        "open-termscript",
        |_ctx, args| super::dispnew::pure::builtin_open_termscript(args),
        1,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("FOpen termscript file: "),
    );
    ctx.defsubr(
        "ding",
        |_ctx, args| super::dispnew::pure::builtin_ding(args),
        0,
        Some(1),
    );
    ctx.defsubr(
        "frame--z-order-lessp",
        |_ctx, args| super::dispnew::pure::builtin_frame_z_order_lessp(args),
        0,
        None,
    );
    ctx.defsubr(
        "force-window-update",
        super::window_cmds::builtin_force_window_update,
        0,
        Some(1),
    );

    // -- Display/terminal --
    ctx.defsubr(
        "x-export-frames",
        |_ctx, args| super::display::builtin_x_export_frames(args),
        0,
        Some(2),
    );
    ctx.defsubr(
        "x-backspace-delete-keys-p",
        |_ctx, args| super::display::builtin_x_backspace_delete_keys_p(args),
        0,
        None,
    );
    ctx.defsubr(
        "x-change-window-property",
        |_ctx, args| super::display::builtin_x_change_window_property(args),
        2,
        Some(7),
    );
    ctx.defsubr(
        "x-focus-frame",
        super::display::builtin_x_focus_frame,
        1,
        Some(2),
    );
    ctx.defsubr(
        "x-get-local-selection",
        |_ctx, args| super::display::builtin_x_get_local_selection(args),
        0,
        Some(2),
    );
    ctx.defsubr(
        "x-get-modifier-masks",
        |_ctx, args| super::display::builtin_x_get_modifier_masks(args),
        0,
        None,
    );
    ctx.defsubr(
        "x-get-selection-internal",
        |_ctx, args| super::display::builtin_x_get_selection_internal(args),
        2,
        Some(4),
    );
    ctx.defsubr(
        "x-display-list",
        |_ctx, args| super::display::builtin_x_display_list(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "x-disown-selection-internal",
        |_ctx, args| super::display::builtin_x_disown_selection_internal(args),
        1,
        Some(3),
    );
    ctx.defsubr(
        "x-delete-window-property",
        |_ctx, args| super::display::builtin_x_delete_window_property(args),
        1,
        Some(3),
    );
    ctx.defsubr(
        "x-frame-edges",
        |_ctx, args| super::display::builtin_x_frame_edges(args),
        0,
        Some(2),
    );
    ctx.defsubr(
        "x-frame-geometry",
        |_ctx, args| super::display::builtin_x_frame_geometry(args),
        0,
        Some(1),
    );
    ctx.defsubr(
        "x-frame-list-z-order",
        |_ctx, args| super::display::builtin_x_frame_list_z_order(args),
        0,
        None,
    );
    ctx.defsubr(
        "x-frame-restack",
        |_ctx, args| super::display::builtin_x_frame_restack(args),
        2,
        Some(3),
    );
    ctx.defsubr(
        "x-family-fonts",
        |_ctx, args| super::display::builtin_x_family_fonts(args),
        0,
        Some(2),
    );
    ctx.defsubr(
        "x-get-atom-name",
        |_ctx, args| super::display::builtin_x_get_atom_name(args),
        0,
        None,
    );
    ctx.defsubr(
        "x-mouse-absolute-pixel-position",
        |_ctx, args| super::display::builtin_x_mouse_absolute_pixel_position(args),
        0,
        None,
    );
    ctx.defsubr(
        "x-own-selection-internal",
        |_ctx, args| super::display::builtin_x_own_selection_internal(args),
        2,
        Some(3),
    );
    ctx.defsubr(
        "x-parse-geometry",
        |_ctx, args| super::display::builtin_x_parse_geometry(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "x-popup-dialog",
        super::display::builtin_x_popup_dialog,
        2,
        Some(3),
    );
    ctx.defsubr(
        "x-popup-menu",
        super::display::builtin_x_popup_menu,
        2,
        Some(2),
    );
    ctx.defsubr(
        "x-register-dnd-atom",
        |_ctx, args| super::display::builtin_x_register_dnd_atom(args),
        0,
        None,
    );
    ctx.defsubr(
        "x-selection-exists-p",
        |_ctx, args| super::display::builtin_x_selection_exists_p(args),
        0,
        None,
    );
    ctx.defsubr(
        "x-selection-owner-p",
        |_ctx, args| super::display::builtin_x_selection_owner_p(args),
        0,
        None,
    );
    ctx.defsubr(
        "x-hide-tip",
        |_ctx, args| super::display::builtin_x_hide_tip(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "x-internal-focus-input-context",
        |_ctx, args| super::display::builtin_x_internal_focus_input_context(args),
        0,
        None,
    );
    ctx.defsubr(
        "x-send-client-message",
        |_ctx, args| super::display::builtin_x_send_client_message(args),
        6,
        Some(6),
    );
    ctx.defsubr(
        "x-show-tip",
        |_ctx, args| super::display::builtin_x_show_tip(args),
        1,
        Some(6),
    );
    ctx.defsubr(
        "x-set-mouse-absolute-pixel-position",
        |_ctx, args| super::display::builtin_x_set_mouse_absolute_pixel_position(args),
        2,
        Some(2),
    );
    ctx.defsubr(
        "x-synchronize",
        |_ctx, args| super::display::builtin_x_synchronize(args),
        1,
        Some(2),
    );
    ctx.defsubr(
        "x-translate-coordinates",
        |_ctx, args| super::display::builtin_x_translate_coordinates(args),
        0,
        None,
    );
    ctx.defsubr(
        "x-uses-old-gtk-dialog",
        |_ctx, args| super::display::builtin_x_uses_old_gtk_dialog(args),
        0,
        None,
    );
    ctx.defsubr(
        "x-window-property",
        |_ctx, args| super::display::builtin_x_window_property(args),
        1,
        Some(6),
    );
    ctx.defsubr(
        "x-window-property-attributes",
        |_ctx, args| super::display::builtin_x_window_property_attributes(args),
        0,
        None,
    );
    ctx.defsubr(
        "x-wm-set-size-hint",
        |_ctx, args| super::display::builtin_x_wm_set_size_hint(args),
        0,
        None,
    );
    ctx.defsubr(
        "terminal-list",
        |_ctx, args| super::terminal::pure::builtin_terminal_list(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "delete-terminal",
        super::terminal::pure::builtin_delete_terminal,
        0,
        Some(2),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "make-terminal-frame",
            super::frame::builtin_make_terminal_frame,
            1,
            Some(1),
        ),
    );

    // -- Image --
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "image-size",
            super::image::builtin_image_size_in_context,
            1,
            Some(3),
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "image-mask-p",
            super::image::builtin_image_mask_p_in_context,
            1,
            Some(2),
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "image-flush",
            super::image::builtin_image_flush_in_context,
            1,
            Some(2),
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "clear-image-cache",
            super::image::builtin_clear_image_cache_in_context,
            0,
            Some(2),
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "image-cache-size",
            super::image::builtin_image_cache_size_in_context,
            0,
            Some(0),
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "image-metadata",
            super::image::builtin_image_metadata_in_context,
            1,
            Some(2),
        ),
    );
    register_builtin(
        ctx,
        BuiltinRegistration::requires_eval_state(
            "neomacs-image-extent",
            super::image::builtin_neomacs_image_extent_in_context,
            1,
            Some(2),
        ),
    );
    ctx.defsubr(
        "imagep",
        |_ctx, args| super::image::builtin_imagep(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "image-transforms-p",
        super::image::builtin_image_transforms_p,
        0,
        Some(1),
    );

    // -- Display engine (xdisp) --
    ctx.defsubr("invisible-p", super::xdisp::builtin_invisible_p, 1, Some(1));
    ctx.defsubr(
        "line-pixel-height",
        |_ctx, args| super::xdisp::builtin_line_pixel_height(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "move-point-visually",
        |_ctx, args| super::xdisp::builtin_move_point_visually(args),
        1,
        Some(1),
    );
    ctx.defsubr(
        "lookup-image-map",
        |_ctx, args| super::xdisp::builtin_lookup_image_map(args),
        3,
        Some(3),
    );
    ctx.defsubr(
        "current-bidi-paragraph-direction",
        super::xdisp::builtin_current_bidi_paragraph_direction,
        0,
        Some(1),
    );
    ctx.defsubr(
        "bidi-resolved-levels",
        |_ctx, args| super::xdisp::builtin_bidi_resolved_levels(args),
        0,
        Some(1),
    );
    ctx.defsubr(
        "bidi-find-overridden-directionality",
        |_ctx, args| super::xdisp::builtin_bidi_find_overridden_directionality(args),
        3,
        Some(4),
    );
    ctx.defsubr_interactive(
        "move-to-window-line",
        super::xdisp::builtin_move_to_window_line,
        1,
        Some(1),
        super::interactive::BuiltinInteractiveSpec::String("P"),
    );
    ctx.defsubr(
        "long-line-optimizations-p",
        |_ctx, args| super::xdisp::builtin_long_line_optimizations_p(args),
        0,
        Some(0),
    );

    // -- XML/decompress --
    ctx.defsubr(
        "libxml-parse-html-region",
        super::xml::builtin_libxml_parse_html_region,
        0,
        Some(4),
    );
    ctx.defsubr(
        "libxml-parse-xml-region",
        super::xml::builtin_libxml_parse_xml_region,
        0,
        Some(4),
    );
    ctx.defsubr(
        "libxml-available-p",
        |_ctx, args| super::xml::builtin_libxml_available_p(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "zlib-available-p",
        |_ctx, args| super::zlib::builtin_zlib_available_p(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "zlib-decompress-region",
        super::zlib::builtin_zlib_decompress_region,
        2,
        Some(3),
    );

    // -- Native compilation compatibility --

    // -- DBus --
    //
    // None.  GNU's six `dbusbind.c' subrs are inside `#ifdef HAVE_DBUS'
    // (src/dbusbind.c:21, syms_of_dbusbind at :2003-2010) and this build links
    // no libdbus.  Ledger 192 deleted the three that stood here: they held no
    // D-Bus code, and answered a hardcoded `2', a fabricated `":1.0"' unique
    // name and an invented `dbus-event' reply from "org.freedesktop.DBus".

    // -- Documentation/help --
    ctx.defsubr(
        "Snarf-documentation",
        super::doc::builtin_snarf_documentation,
        1,
        Some(1),
    );

    // -- JSON --
    ctx.defsubr(
        "json-serialize",
        |_ctx, args| super::json::builtin_json_serialize(args),
        1,
        None,
    );
    ctx.defsubr(
        "json-parse-string",
        |_ctx, args| super::json::builtin_json_parse_string(args),
        1,
        None,
    );

    // -- Composite --
    ctx.defsubr(
        "compose-string-internal",
        |_ctx, args| super::composite::builtin_compose_string_internal(args),
        3,
        Some(5),
    );
    ctx.defsubr(
        "find-composition-internal",
        super::composite::builtin_find_composition_internal,
        4,
        Some(4),
    );
    ctx.defsubr(
        "composition-get-gstring",
        super::composite::builtin_composition_get_gstring,
        4,
        Some(4),
    );
    ctx.defsubr(
        "clear-composition-cache",
        |_ctx, args| super::composite::builtin_clear_composition_cache(args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "composition-sort-rules",
        |_ctx, args| super::composite::builtin_composition_sort_rules(args),
        1,
        Some(1),
    );

    // -- Marker --
    ctx.defsubr(
        "markerp",
        |_ctx, args| super::marker::builtin_markerp(args),
        1,
        Some(1),
    );

    // -- Lread --
    ctx.defsubr(
        "get-load-suffixes",
        |ctx, args| super::lread::builtin_get_load_suffixes(&ctx.obarray, args),
        0,
        Some(0),
    );
    ctx.defsubr(
        "read-coding-system",
        super::lread::builtin_read_coding_system,
        1,
        Some(2),
    );
    ctx.defsubr(
        "read-non-nil-coding-system",
        super::lread::builtin_read_non_nil_coding_system,
        1,
        Some(1),
    );

    // -- Base64/hash --
    ctx.defsubr(
        "base64-encode-string",
        |_ctx, args| super::fns::builtin_base64_encode_string(args),
        1,
        Some(2),
    );
    ctx.defsubr(
        "base64-decode-string",
        |_ctx, args| super::fns::builtin_base64_decode_string(args),
        1,
        Some(3),
    );
    ctx.defsubr(
        "base64url-encode-string",
        |_ctx, args| super::fns::builtin_base64url_encode_string(args),
        1,
        Some(2),
    );

    // -- Window builtins: `switch-to-buffer' (lisp/window.el:9558),
    // `display-buffer' (:8166) and `pop-to-buffer' (:9403) are Lisp and only
    // Lisp (DIVERGENCES.md 154).  The C primitives underneath them --
    // `set-window-buffer', `select-window', `set-buffer' -- stay registered.

    // -- Window tree / resize: `balance-windows' (lisp/window.el:6222),
    // `enlarge-window' (:3714), `shrink-window' (:3759) and `window-tree'
    // (:3999) are Lisp and only Lisp (DIVERGENCES.md 154).  They are written
    // over `window-resize-apply', `window-resize-apply-total' and
    // `frame-root-window', which are C DEFUNs and stay registered.

    // GNU exposes public evaluator-owned entries like `if` and `throw` as
    // real subrs in the function cell even though they are dispatched by the
    // evaluator rather than the ordinary builtin function table.
    symbols::init_event_symbol_properties(&mut ctx.obarray);
    ctx.materialize_public_evaluator_function_cells();
}
