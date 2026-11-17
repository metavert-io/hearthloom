//! For tests: how much work the store's connection does while something
//! runs, read from SQLite's own counters — the statements it runs, and the
//! steps of its virtual machine (rows looked at and written, roughly) for
//! each.

use std::os::raw::{c_int, c_uint, c_void};
use std::sync::Mutex;

use super::Store;

/// What a run cost: each statement that finished, with its steps.
#[derive(Default)]
pub(crate) struct Work {
    finished: Mutex<Vec<(String, i64)>>,
}

impl Work {
    pub(crate) fn statements(&self) -> usize {
        self.finished.lock().unwrap().len()
    }

    /// How many times the statement `sql` ran.
    pub(crate) fn runs_of(&self, sql: &str) -> usize {
        self.finished
            .lock()
            .unwrap()
            .iter()
            .filter(|(text, _)| text == sql)
            .count()
    }

    /// The steps of the statements whose text names `table`.
    pub(crate) fn steps_on(&self, table: &str) -> i64 {
        self.finished
            .lock()
            .unwrap()
            .iter()
            .filter(|(sql, _)| sql.contains(table))
            .map(|(_, steps)| steps)
            .sum()
    }
}

/// Called as each statement finishes (`SQLITE_TRACE_PROFILE`), with the
/// statement in `stmt`.
unsafe extern "C" fn finished(
    _: c_uint,
    work: *mut c_void,
    stmt: *mut c_void,
    _: *mut c_void,
) -> c_int {
    // SAFETY: `work` is the `Work` handed to `sqlite3_trace_v2` below, alive
    // until tracing stops; `stmt` is the statement that just finished
    unsafe {
        let work = &*(work as *const Work);
        let stmt = stmt as *mut rusqlite::ffi::sqlite3_stmt;
        let steps =
            rusqlite::ffi::sqlite3_stmt_status(stmt, rusqlite::ffi::SQLITE_STMTSTATUS_VM_STEP, 1);
        let sql = rusqlite::ffi::sqlite3_sql(stmt);
        let sql = if sql.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(sql).to_string_lossy().into_owned()
        };
        if let Ok(mut finished) = work.finished.lock() {
            finished.push((sql, i64::from(steps)));
        }
    }
    0
}

/// The work `f` makes the store's connection do.
pub(crate) fn work_of(store: &Store, f: impl FnOnce()) -> Work {
    let work = Work::default();
    // SAFETY: the handle is the store's open connection, and `work`
    // outlives the trace, which is taken off before it goes
    unsafe {
        rusqlite::ffi::sqlite3_trace_v2(
            store.lock().handle(),
            rusqlite::ffi::SQLITE_TRACE_PROFILE as c_uint,
            Some(finished),
            &work as *const Work as *mut c_void,
        );
    }
    f();
    unsafe {
        rusqlite::ffi::sqlite3_trace_v2(store.lock().handle(), 0, None, std::ptr::null_mut());
    }
    work
}
