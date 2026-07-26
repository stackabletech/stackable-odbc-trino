//! End-to-end fetch-path benchmarks for the Trino backend.
//!
//! Opt-in: requires a running Trino coordinator. Skips silently if
//! `TRINO_BENCH_URL` is unset (so `cargo bench --workspace` still works on a
//! plain checkout).
//!
//! Run:
//!   ./test/trino/setup.sh
//!   TRINO_BENCH_URL=http://localhost:8080 cargo bench -p stackable-odbc-trino
//!
//! Override workload size:
//!   TRINO_BENCH_URL=http://localhost:8080 BENCH_ROWS=1000000 cargo bench -p stackable-odbc-trino
//!
//! The default query uses Trino's `SEQUENCE` builtin and does not require any
//! particular catalog. To override:
//!   TRINO_BENCH_QUERY="SELECT * FROM tpcds.tiny.store_sales LIMIT 100000" \
//!     TRINO_BENCH_URL=http://localhost:8080 cargo bench -p stackable-odbc-trino

use std::ffi::c_void;
use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use stackable_odbc_core::ffi;
use stackable_odbc_core::types::{CDataType, FreeStmtOption, HandleType, SqlReturn};
use stackable_odbc_trino::TrinoBackend;

#[derive(Clone)]
struct BenchConfig {
    rows: usize,
    cols: usize,
    repeat_get_data: usize,
    url: String,
    query_override: Option<String>,
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Returns `None` if `TRINO_BENCH_URL` is unset — caller should print a skip
/// message and return early.
fn bench_config() -> Option<BenchConfig> {
    let url = std::env::var("TRINO_BENCH_URL").ok()?;
    Some(BenchConfig {
        rows: env_or("BENCH_ROWS", 100_000),
        cols: env_or("BENCH_COLS", 20),
        repeat_get_data: env_or("BENCH_REPEAT_GET_DATA", 3),
        url,
        query_override: std::env::var("TRINO_BENCH_QUERY").ok(),
    })
}

fn configure_for_size(c: Criterion, rows: usize) -> Criterion {
    if rows > 250_000 {
        c.sample_size(20)
            .measurement_time(Duration::from_secs(60))
            .warm_up_time(Duration::from_secs(5))
    } else {
        c
    }
}

fn shape_a_split(n_cols: usize) -> (usize, usize, usize) {
    let s = (n_cols * 4) / 10;
    let d = n_cols / 10;
    let i = n_cols - s - d;
    debug_assert_eq!(i + s + d, n_cols, "shape_a_split must total n_cols");
    (i, s, d)
}

/// Generate Shape A as a Trino SQL query backed by SEQUENCE.
fn shape_a_query(rows: usize, cols: usize) -> String {
    let (n_i, n_s, n_d) = shape_a_split(cols);
    let mut select_exprs: Vec<String> = Vec::with_capacity(cols);
    for i in 0..n_i {
        select_exprs.push(format!("(n + {i}) AS i_{i}"));
    }
    for i in 0..n_s {
        // Fixed 32-char string: a stable payload size keeps row widths
        // comparable across benchmark runs.
        select_exprs.push(format!(
            "CAST('xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx' AS VARCHAR) AS s_{i}"
        ));
    }
    for i in 0..n_d {
        // Fixed decimal value, for the same reason.
        select_exprs.push(format!("CAST(12345678.90 AS DECIMAL(18,2)) AS d_{i}"));
    }
    format!(
        "SELECT {} FROM UNNEST(SEQUENCE(1, {rows})) AS t(n)",
        select_exprs.join(", "),
    )
}

/// Build the connection string. Trino setup.sh uses `Host=localhost;Port=8080;User=admin;Protocol=http`.
fn connect_string(url: &str) -> String {
    // Parse `http://host:port` minimally; reject unsupported forms loudly.
    let (proto, rest) = url.split_once("://").unwrap_or(("http", url));
    let (host, port) = rest.split_once(':').unwrap_or((rest, "8080"));
    format!("Host={host};Port={port};User=admin;Protocol={proto}")
}

unsafe fn alloc_handles() -> (*mut c_void, *mut c_void, *mut c_void) {
    unsafe {
        let mut env: *mut c_void = std::ptr::null_mut();
        let _ = ffi::handle::sql_alloc_handle::<TrinoBackend>(
            HandleType::Env as i16,
            std::ptr::null_mut(),
            &mut env,
        );
        let mut conn: *mut c_void = std::ptr::null_mut();
        let _ =
            ffi::handle::sql_alloc_handle::<TrinoBackend>(HandleType::Dbc as i16, env, &mut conn);
        let mut stmt: *mut c_void = std::ptr::null_mut();
        let _ =
            ffi::handle::sql_alloc_handle::<TrinoBackend>(HandleType::Stmt as i16, conn, &mut stmt);
        (env, conn, stmt)
    }
}

unsafe fn connect_trino(conn: *mut c_void, conn_str: &str) -> SqlReturn {
    unsafe {
        let wide: Vec<u16> = conn_str.encode_utf16().collect();
        ffi::connect::sql_driver_connect_w::<TrinoBackend>(
            conn,
            std::ptr::null_mut(),
            wide.as_ptr(),
            wide.len() as i16,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            0,
        )
    }
}

unsafe fn exec_direct(stmt: *mut c_void, sql: &str) -> SqlReturn {
    unsafe {
        let wide: Vec<u16> = sql.encode_utf16().collect();
        ffi::execute::sql_exec_direct_w::<TrinoBackend>(stmt, wide.as_ptr(), wide.len() as i32)
    }
}

unsafe fn cleanup(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
    unsafe {
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Stmt as i16, stmt);
        let _ = ffi::connect::sql_disconnect::<TrinoBackend>(conn);
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Dbc as i16, conn);
        let _ = ffi::handle::sql_free_handle::<TrinoBackend>(HandleType::Env as i16, env);
    }
}

unsafe fn drain_late_binding(stmt: *mut c_void, n_cols: u16) -> usize {
    unsafe {
        let mut buf = vec![0u8; 4096];
        let mut ind: isize = 0;
        let mut count = 0usize;
        while ffi::fetch::sql_fetch::<TrinoBackend>(stmt) == SqlReturn::SUCCESS {
            for col in 1..=n_cols {
                let _ = ffi::fetch::sql_get_data::<TrinoBackend>(
                    stmt,
                    col,
                    CDataType::Default as i16,
                    buf.as_mut_ptr() as *mut c_void,
                    buf.len() as isize,
                    &mut ind,
                );
                count += 1;
            }
        }
        count
    }
}

struct Bound {
    buf: Vec<u8>,
    ind: isize,
}

/// Bind every column once; returns the binding storage. Caller must keep the
/// returned Vec alive for as long as the bindings are in effect.
unsafe fn bind_columns_trino(stmt: *mut c_void, n_cols: u16) -> Vec<Bound> {
    unsafe {
        let mut bindings: Vec<Bound> = (0..n_cols)
            .map(|_| Bound {
                buf: vec![0u8; 4096],
                ind: 0,
            })
            .collect();
        for (i, b) in bindings.iter_mut().enumerate() {
            let _ = ffi::bind::sql_bind_col::<TrinoBackend>(
                stmt,
                (i + 1) as u16,
                CDataType::Default as i16,
                b.buf.as_mut_ptr() as *mut c_void,
                b.buf.len() as isize,
                &mut b.ind,
            );
        }
        bindings
    }
}

/// Drain a result set whose columns have already been bound via `bind_columns_trino`.
/// Returns total cells fetched (n_cols × n_rows).
unsafe fn drain_bound_columns(stmt: *mut c_void, n_cols: u16) -> usize {
    unsafe {
        let mut count = 0usize;
        while ffi::fetch::sql_fetch::<TrinoBackend>(stmt) == SqlReturn::SUCCESS {
            count += n_cols as usize;
        }
        count
    }
}

unsafe fn drain_repeat_get_data(stmt: *mut c_void, n_cols: u16, repeats: usize) -> usize {
    unsafe {
        let mut buf = vec![0u8; 4096];
        let mut ind: isize = 0;
        let mut count = 0usize;
        while ffi::fetch::sql_fetch::<TrinoBackend>(stmt) == SqlReturn::SUCCESS {
            for col in 1..=n_cols {
                for _ in 0..repeats {
                    let _ = ffi::fetch::sql_get_data::<TrinoBackend>(
                        stmt,
                        col,
                        CDataType::Default as i16,
                        buf.as_mut_ptr() as *mut c_void,
                        buf.len() as isize,
                        &mut ind,
                    );
                    count += 1;
                }
            }
        }
        count
    }
}

fn bench_trino(c: &mut Criterion) {
    let Some(cfg) = bench_config() else {
        eprintln!("[fetch_trino] TRINO_BENCH_URL unset — skipping Trino benches.");
        return;
    };

    let conn_str = connect_string(&cfg.url);
    let query = cfg
        .query_override
        .clone()
        .unwrap_or_else(|| shape_a_query(cfg.rows, cfg.cols));
    let label = format!("{}x{}", cfg.rows, cfg.cols);

    let (env, conn, stmt) = unsafe { alloc_handles() };
    let connect_ret = unsafe { connect_trino(conn, &conn_str) };
    if connect_ret != SqlReturn::SUCCESS {
        unsafe { cleanup(env, conn, stmt) };
        eprintln!(
            "[fetch_trino] Connect to {} failed (SqlReturn {:?}); skipping.",
            cfg.url, connect_ret
        );
        return;
    }

    let mut group = c.benchmark_group("trino/shape_a");
    group.throughput(Throughput::Elements((cfg.rows * cfg.cols) as u64));

    group.bench_function(BenchmarkId::new("late_binding", &label), |b| {
        b.iter_batched(
            || unsafe {
                let _ = ffi::cursor::sql_close_cursor::<TrinoBackend>(stmt);
                assert_eq!(exec_direct(stmt, &query), SqlReturn::SUCCESS);
            },
            |_| {
                black_box(unsafe { drain_late_binding(stmt, cfg.cols as u16) });
            },
            BatchSize::PerIteration,
        );
    });

    group.bench_function(BenchmarkId::new("bound_columns", &label), |b| {
        // Close the cursor left open by the late_binding bench's last iteration.
        let _ = unsafe { ffi::cursor::sql_close_cursor::<TrinoBackend>(stmt) };
        // Bind columns once, before the bench loop. Bindings survive SQLCloseCursor
        // per ODBC spec, so they remain in effect across all iterations.
        assert_eq!(unsafe { exec_direct(stmt, &query) }, SqlReturn::SUCCESS);
        let _bindings = unsafe { bind_columns_trino(stmt, cfg.cols as u16) };
        let _ = unsafe { ffi::cursor::sql_close_cursor::<TrinoBackend>(stmt) };

        b.iter_batched(
            || unsafe {
                let _ = ffi::cursor::sql_close_cursor::<TrinoBackend>(stmt);
                assert_eq!(exec_direct(stmt, &query), SqlReturn::SUCCESS);
            },
            |_| {
                black_box(unsafe { drain_bound_columns(stmt, cfg.cols as u16) });
            },
            BatchSize::PerIteration,
        );
    });

    let repeat_id = BenchmarkId::new(format!("repeat_get_data_x{}", cfg.repeat_get_data), &label);
    group.bench_function(repeat_id, |b| {
        // Unbind columns from the bound_columns bench — its buffers are freed
        // when that closure exits, so fetching with stale bindings is UB.
        let _ = unsafe {
            ffi::handle::sql_free_stmt::<TrinoBackend>(stmt, FreeStmtOption::Unbind as u16)
        };
        b.iter_batched(
            || unsafe {
                let _ = ffi::cursor::sql_close_cursor::<TrinoBackend>(stmt);
                assert_eq!(exec_direct(stmt, &query), SqlReturn::SUCCESS);
            },
            |_| {
                black_box(unsafe {
                    drain_repeat_get_data(stmt, cfg.cols as u16, cfg.repeat_get_data)
                });
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();

    unsafe { cleanup(env, conn, stmt) };
}

fn benches() -> Criterion {
    let rows = bench_config().map(|c| c.rows).unwrap_or(0);
    configure_for_size(Criterion::default(), rows)
}

criterion_group! {
    name = benches_group;
    config = benches();
    targets = bench_trino
}
criterion_main!(benches_group);
