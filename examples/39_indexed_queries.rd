// SLICES 32–35: walks, comprehensions over state, aggregations, and
// auto-maintained indexes — the SQL-replacement story end-to-end.
//
// Example: a tiny employee directory. Two index shapes flexed:
//
//   * `unique_index by_email on employees.email` — one primary key
//     per email. The compiler emits a check-before-write at every
//     primary insert: writing a different id under an email that's
//     already indexed aborts the tx with a unique-constraint error.
//     Re-writing the same (email, id) pair is idempotent.
//
//   * `index by_dept on employees.dept` — many primary keys per
//     department. Index slot is `pmap<F, [K]>`; auto-maintenance
//     reads, dedup-appends, and writes back. Re-registering the
//     same id doesn't duplicate the entry.
//
// SQL analogs: `CREATE UNIQUE INDEX users_by_email ON users(email)`
// and `CREATE INDEX users_by_dept ON users(dept)`. Same default
// (non-unique) and same opt-in to uniqueness via a keyword.
//
// Every query function below is `view`. The host can serve them on
// the read-only path (`Engine::query`) — no OCC bookkeeping, no
// write log. The walks classify as ReadOnly, the aggregations are
// pure folds over the walk's array result, and the cluster planner
// doesn't fence around them.

module directory;

struct Employee {
    id:        u64,
    name:      string,
    email:     string,
    dept:      string,
    hire_year: u64,
    salary:    u64,
}

state employees: pmap<u64, Employee>;
state by_email:  pmap<string, u64>;
state by_dept:   pmap<string, [u64]>;
state by_id:     pbtree<u64, u64>;        // sorted unique index for paging

unique_index by_email on employees.email;
index        by_dept  on employees.dept;
unique_index by_id    on employees.id;    // pbtree slot → range queries

// ---------- mutators ----------

entry fn register(
    id: u64, name: string, email: string,
    dept: string, hire_year: u64, salary: u64,
) -> u64 {
    // The compiler emits `by_email[email] = id` automatically
    // alongside the primary write. No second statement needed; the
    // index can't drift out of sync with the primary because we
    // can't write to the primary without also updating the index.
    employees[id] = Employee {
        id: id,
        name: name,
        email: email,
        dept: dept,
        hire_year: hire_year,
        salary: salary,
    };
    return id;
}

// Raise an employee's salary. The indexed `email` field is
// unchanged, so the index stays correct (no stale-entry concern —
// see docs/language/indexes.md for the field-changes-on-update
// caveat that doesn't apply here).
entry fn give_raise(id: u64, amount: u64) -> u64 {
    let cur = employees[id];
    employees[id] = Employee {
        id:        cur.id,
        name:      cur.name,
        email:     cur.email,
        dept:      cur.dept,
        hire_year: cur.hire_year,
        salary:    cur.salary + amount,
    };
    return cur.salary + amount;
}

// ---------- queries: indexed lookup ----------

// O(log32 N) walk through `by_email`'s HAMT, then one direct lookup
// in `employees`. This is the "use the index" path — analogous to
// SQL's `WHERE email = ?` with an index on email.
entry view fn find_by_email(email: string) -> Employee {
    let id = by_email[email];
    return employees[id];
}

// ---------- queries: indexed group lookup ----------

// `by_dept[dept]` gives the list of ids in that dept directly,
// without scanning the whole employees pmap. For a department of
// size M out of N total employees, this is M reads instead of N.
entry view fn list_in_dept_indexed(dept: string) -> [Employee] {
    return [employees[id] for id in by_dept[dept]];
}

// Aggregation against the index list — the SQL `SUM(...) ... GROUP
// BY dept` shape, with the GROUP BY happening at write-time and the
// SUM happening at read-time over the cached id list.
entry view fn dept_payroll_indexed(dept: string) -> u64 {
    return sum([employees[id].salary for id in by_dept[dept]], 0u64);
}

// ---------- queries: scan with predicate ----------

// The same shape works without the index — the planner can still
// serve this on the read-only path. Use the indexed form when the
// dept set is small relative to the full employee count; the scan
// when it isn't, or when you have multiple correlated conditions.
entry view fn list_in_dept(dept: string) -> [Employee] {
    return [u for u in employees if u.dept == dept];
}

// Same shape, projecting only the names. Cheaper to ship across
// the wire than full records.
entry view fn names_in_dept(dept: string) -> [string] {
    return [u.name for u in employees if u.dept == dept];
}

// Newer-than. Comprehension predicates compose just like SQL
// `WHERE` clauses.
entry view fn hired_since(year: u64) -> [Employee] {
    return [u for u in employees if u.hire_year >= year];
}

// ---------- queries: aggregations ----------

// SQL: SELECT SUM(salary) FROM employees;
entry view fn total_payroll() -> u64 {
    return sum([u.salary for u in employees], 0u64);
}

// SQL: SELECT MAX(salary) FROM employees WHERE dept = ?;
// The `0u64` second argument is the empty-array default, never
// returned for non-empty arrays.
entry view fn top_salary(dept: string) -> u64 {
    return max([u.salary for u in employees if u.dept == dept], 0u64);
}

// SQL: SELECT MIN(salary) FROM employees;
entry view fn min_salary() -> u64 {
    return min([u.salary for u in employees], 0u64);
}

// SQL: SELECT COUNT(*) FROM employees WHERE hire_year >= ?;
// `len` over a filtered comprehension is the COUNT-WHERE idiom.
entry view fn count_hired_since(year: u64) -> i64 {
    return len([u for u in employees if u.hire_year >= year]);
}

// SQL: SELECT AVG(salary) FROM employees WHERE dept = ?;
// Built from sum/len primitives. Returns 0 for empty depts to
// sidestep div-by-zero.
entry view fn avg_salary(dept: string) -> u64 {
    let in_dept = [u.salary for u in employees if u.dept == dept];
    let n = u64(len(in_dept));
    if n == 0u64 { return 0u64; }
    return sum(in_dept, 0u64) / n;
}

// ---------- queries: paginated through sorted index ----------

// `by_id` is a `pbtree<u64, u64>` — a unique index over a sorted
// trie. `pbtree_range` returns primary keys (employee ids) in the
// requested range, in sorted order. This is the SQL
// `SELECT id FROM employees WHERE id BETWEEN lo AND hi ORDER BY id`
// shape, served on the read-only `Engine::query` path.
entry view fn page_ids(after_id: u64, n: u64) -> [u64] {
    return pbtree_range(by_id, after_id, after_id + n);
}

// Streaming sorted iteration: walk employees in id order with
// `break` for early exit. The first match is returned without
// fetching unrelated subtrees.
entry view fn first_id() -> u64 {
    for id in by_id {
        return id;
    }
    return 0u64;
}

// ---------- queries: indexed-then-aggregated ----------

// Mix the two shapes: index-lookup the email→id, then check a
// derived condition. The index lookup is the fast filter; the
// downstream check is constant-time on a single record.
entry view fn is_in_dept_by_email(email: string, dept: string) -> bool {
    let id = by_email[email];
    return employees[id].dept == dept;
}

// ---------- constructor ----------

// Seed sample data at deploy time so a fresh KV has something for
// the first wave of queries.
fn main() -> u64 {
    register(1u64, "alice", "alice@x.com", "eng",   2020u64, 150000u64);
    register(2u64, "bob",   "bob@x.com",   "eng",   2021u64, 130000u64);
    register(3u64, "carol", "carol@x.com", "sales", 2019u64, 200000u64);
    register(4u64, "dan",   "dan@x.com",   "sales", 2022u64, 110000u64);
    register(5u64, "eve",   "eve@x.com",   "eng",   2023u64, 140000u64);
    return 5u64;
}
