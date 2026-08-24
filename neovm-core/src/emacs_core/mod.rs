//! Elisp interpreter module.
//!
//! Provides a full Elisp evaluator with:
//! - Value types: nil, t, int, float, string, symbol, keyword, char, cons, vector, hash-table
//! - Complete parser: strings, floats, chars, vectors, dotted pairs, quasiquote, reader macros
//! - Special forms: quote, function, let, let*, setq, if, and, or, cond, while, progn, prog1,
//!   lambda, defun, defvar, defconst, defmacro, funcall, catch, throw, unwind-protect,
//!   condition-case, when, unless
//! - 100+ built-in functions: arithmetic, comparisons, type predicates, list ops, string ops,
//!   vector ops, hash tables, higher-order functions, conversion, property lists

pub mod abbrev;
pub mod advice;
pub mod alloc;
pub mod autoload;
pub mod bookmark;
pub mod buffer;
pub mod buffer_vars;
pub mod builtins;
pub mod builtins_extra;
pub mod bytecode;
pub mod callproc;
pub mod casefiddle;
pub mod casetab;
pub mod category;
pub mod ccl;
pub mod character;
pub mod charset;
pub mod chartable;
pub mod chrome_dirty;
pub mod cl_lib;
pub mod coding;
pub mod comp;
#[cfg(test)]
pub mod compat_regressions;
pub mod composite;
pub mod cus_start_platform_vars;
pub mod custom;
pub mod daemon;
#[cfg(test)]
mod daemon_test;
pub mod data;
pub mod dbus;
pub mod debug;
pub mod debug_on_call;
pub mod defvar_bool;
pub mod defvar_object;
pub mod dired;
pub mod display;
pub mod display_host;
pub mod display_spec;
pub mod dispnew;
pub mod doc;
pub mod dynamic_module;
pub mod editfns;
pub mod effect_profile;
pub mod emacs_char;
pub(crate) mod environment;
pub mod error;
pub mod errors;
pub mod eval;
pub mod fileio;
pub mod filelock;
pub mod floatfns;
pub mod fns;
pub mod font;
pub mod fontset;
pub mod format;
pub mod forward;
pub mod frame;
pub mod frame_vars;
pub mod gc_stats;
#[cfg(test)]
mod gnu_defvar_special_test;
pub mod hashtab;
pub(crate) mod hook_runtime;
pub mod hscroll;
pub mod image;
pub mod image_catalog;
pub mod image_path;
pub mod indent;
pub mod interactive;
pub mod intern;
pub mod jit;
pub mod json;
pub mod kbd;
pub mod keyboard;
pub mod keymap;
#[cfg(test)]
mod kill_ring_test;
pub mod kmacro;
pub mod load;
pub mod lread;
pub mod marker;
pub mod minibuffer;
pub mod misc;
pub mod mode;
pub mod navigation;
pub(crate) mod neo;
pub mod network;
pub(crate) mod os_signal;
pub mod pdump;
pub mod perf_trace;
pub mod plist;
pub(crate) mod position;
pub(crate) mod post_image_init;
pub(crate) mod prefix;
pub mod print;
pub mod process;
pub(crate) mod profiler;
#[cfg(test)]
mod quit_regression_test;
pub mod reader;
pub mod rect;
pub mod regex;
pub mod regex_emacs;
pub mod register;
pub(crate) mod runtime_identity;
#[cfg(test)]
mod runtime_string_guard_test;
pub mod search;
pub mod shader_surface;
#[cfg(test)]
mod shader_surface_test;
pub(crate) mod sound;
pub(crate) mod sqlite;
pub(crate) mod string_escape;
pub mod subr_docs;
pub mod subr_info;
pub mod symbol;
#[cfg(test)]
mod symbol_function_regression_test;
#[cfg(test)]
mod symbol_plist_regression_test;
#[cfg(test)]
mod symbol_redirect_regression_test;
pub mod syntax;
#[cfg(test)]
mod syntax_category_property_test;
#[cfg(test)]
mod syntax_gnu_parity_regression_test;
pub mod terminal;
pub mod textprop;
pub mod threads;
pub mod timefns;
pub mod timer;
pub(crate) mod tls;
#[cfg(test)]
mod tls_test;
pub mod treesit;
pub mod undo;
pub mod value;
pub mod value_reader;
pub mod var_docs;
pub(crate) mod w32;
pub(crate) mod wait;
pub mod window_cmds;
#[cfg(test)]
mod window_system_preload_test;
pub mod xdisp;
pub mod xfaces;
pub mod xml;
pub mod xwidget;
#[cfg(test)]
mod xwidget_test;
pub(crate) mod zlib;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MenuBarPopupAnchor {
    pub(crate) frame_id: crate::window::FrameId,
    pub(crate) menu_key: Option<String>,
    pub(crate) menu_x: i64,
    pub(crate) x: i64,
    pub(crate) y: i64,
    pub(crate) width: i64,
    pub(crate) height: i64,
}

// Re-export the main public API
pub use bytecode::{ByteCodeFunction, Vm as ByteVm};
pub use display_host::DisplayHost;
pub use error::{
    EvalError, format_eval_result, format_eval_result_bytes_with_eval,
    format_eval_result_with_eval, print_value_bytes_with_eval, print_value_with_eval,
};
pub use eval::{Context, GuiFrameHostRequest, PopupMenuEntry, PopupMenuRequest};
pub use intern::SymId;
pub use print::{print_value, print_value_bytes, print_value_with_buffers};
pub use symbol::Obarray;
pub use value::{LambdaData, LambdaParams, Value, ValueKind, VecLikeType};

/// Convenience: parse and evaluate source code, returning the last form's value.
pub fn eval_source(input: &str) -> Result<Value, EvalError> {
    let mut evaluator = Context::new();
    evaluator.eval_str(input)
}
