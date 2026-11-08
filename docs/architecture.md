# How pos3ql works

pos3ql serves PostgreSQL SQL and protocol clients while saving durable state in
an S3-compatible object store. A configured object prefix contains one pos3ql
instance's durable state, including its SQL databases. One active process owns
the right to publish changes to that prefix. This is a single active writer
process, not a rule that every storage request must run in sequence.

**Example:** the same instance can contain `orders` and `billing` databases.
Their tables use the same startup memory budget and object-store namespace.
A backup or writer replacement concerns that namespace; creating a SQL database
does not create an independently operated storage service.

Startup may take exclusive recovery ownership before validating the remaining
journal. This prevents the previous writer from publishing while recovery runs;
a failed recovery can therefore leave that writer fenced out. The new process
accepts SQL writes only after journal replay and restored definitions validate.

## The request path

```mermaid
flowchart LR
    Client[PostgreSQL client] --> Server[Connection buffers and one SQL CPU worker]
    Server --> Rows[Transaction state and cached rows]
    Server --> Reads[Reserved object read slots]
    Reads --> Objects[Immutable data and index objects]
    Rows --> Log[Private journal records]
    Log --> Upload[Batch bytes and descriptor uploads]
    Upload --> Head[Conditional commit-head update]
    Head --> Commit[Install complete transaction and return success]
    Rows --> Save[Checkpoint and merge work]
    Save --> Objects
    Save --> Manifest[Conditional checkpoint-manifest update]
```

Connection input, parsed SQL, statement workspaces, transaction undo, locks,
object requests and output have separate fixed reservations. Catalog and derived
result descriptions retain their actual columns, rather than maximum-width table
definitions, in statement memory. Execution schemas share the existing startup
image pool.
After startup, ordinary runtime work cannot grow a heap, pool or sort allocation. The isolated
TLS transport uses its separately budgeted pool. The memory plan describes
these reservations; it is not the entire operating-system RSS, kernel socket
memory or the memory used by an independently operated object-storage service.

A reactor waits for sockets and deadlines, then advances ready connections and
maintenance. After each maintenance turn and any queries it wakes, the loop
registers newly opened storage sockets before sleeping. Cleanup therefore keeps
moving on an idle server without requiring another query to wake it.
SQL CPU work currently runs on one worker. An eligible read that
needs a download yields with its original data owners and command deadline;
another connection can run cached work. Adding connections or
`query_workspace_slots` does not add CPU execution workers.

A waiting command keeps the downloads it needs. Completion or failure releases
unused read-ahead requests. Commit does so once all required row images are in
the journal, before waiting for its durable receipt; an abandoned speculative
download cannot hold up the next checkpoint.

State-changing routines, procedure commit boundaries, administrative work and
large-result reads after their first transport flush still have synchronous
reads. A cold read on one
of these paths can occupy that worker. [PLAN.md](../PLAN.md) tracks their
conversion and the later CPU worker pool separately.

Function calls bind their selected function, input mapping and declared types
before running arguments. At execution, reached inputs and defaults run in
declaration order; lookup never evaluates an argument as a probe. Defaults keep the object identities
captured when the routine was created. A renamed sequence therefore cannot be
replaced accidentally by another sequence with its old name.

Lookup retains explicit built-in namespaces and compares compatible user and
built-in function signatures before selecting a call. An exact input-type match
can beat a generic match in an earlier schema; identical signatures follow
`search_path` order. Result fields come from the selected function's declaration,
including empty table-function results used by client metadata queries.
For example, with `search_path=public,pg_catalog`, a user's `sum(integer)`
can produce one value per input row; selecting the built-in aggregate instead
produces one value per group. Planning follows the selected declaration.
SQL constructors carry an explicit syntax tag through parsing and rewrites;
a quoted routine name cannot imitate an internal operator or JSON constructor.
The callable declarations come from PostgreSQL 18.6. Static name/OID indexes
and streamed candidates avoid a growing candidate list or runtime sort.
Planner safety facts also cover internal type-conversion procedures from the
pinned PostgreSQL catalog sources. For example, numeric text input is immutable,
while date input can depend on session settings. These facts guide planning;
they do not make additional procedures callable from SQL.

Conversion lookup also uses declared types, including typed NULL values, before
input effects. Each array child converts before the following child runs. For
`INSERT ... SELECT`, the SELECT's sorting and limits determine its output; each
complete output row then converts before the next row is produced. For example,
an overflowing first array child must not advance a sequence in a later child.

Some literal values are checked while SQL is prepared. For example, preparing
`SELECT '{approved}'::checked_status[]` can run checks on a domain, a type with
extra constraints. The same applies when an untyped string gets that array type
from a column or function parameter. The parser records each occurrence's
position, selected type identity and encoded value. Describe, Execute and
retries restore that value without repeating its checks. Equal literals at
different positions remain distinct inputs. A typed text expression or an
`ARRAY[...]` constructor keeps its later execution-time checks.

Checked values belong to the SQL source that analyzed them:

| Source | Input preparation and lifetime |
|---|---|
| Simple command | Prepared at command admission; a new command analyzes its inputs again. |
| SQL `PREPARE` or protocol Parse | Prepared once and retained with that statement and its bound portals. Describe and repeated execution reuse the values. |
| SQL string function or procedure body | `CREATE` validation is separate. The first runtime call prepares inputs retained by the backend for the routine definition and resolved call types. Later commands reuse them. |
| SQL function considered for inlining | The caller's planner checks the body. An accepted inline keeps its checked values with that caller plan; a rejected attempt leaves runtime preparation separate. |
| Supported static PL/pgSQL source | Prepared when reached and retained by the backend for that routine definition, resolved input types, input collation and source occurrence. An unreached branch does not prepare its inputs. |
| Dynamic `EXECUTE` | Each execution owns a fresh plan, even when the command text equals an earlier command. |
| Stored view, rule, default or SQL `BEGIN ATOMIC` body | Prepared with the definition and retained in its immutable metadata image, including domain and view-column defaults. Recovery restores values and captured type identities without rerunning checks. |

For example, an opaque SQL function containing a checked array literal checks
once at `CREATE`, then once at its first runtime call in that backend. A later
`SELECT` reuses that runtime value. A function considered for inlining can
check during planning even when the caller returns no rows; a rejected attempt
does not replace the first runtime check. Cursor plans retain accepted inputs
across separate `FETCH` commands and while a `WITH HOLD` cursor survives commit.

Planning uses facts from all 3,397 procedures in the pinned PostgreSQL catalog,
including internal operator, cast and type-input functions. For example, checking
whether a SQL function can be inlined needs the properties of text concatenation,
even though its internal `textcat` function is not exposed as a SQL call.
Initialized callable declarations take precedence over bootstrap facts. Adding
these facts does not add callable functions; [source hashes and generation
details](https://github.com/e6qu/pos3ql/blob/d79ba0232874d6dfbd7a9f7a744af9a9a12f3318/tests/external/reference/2026-11-03-input-plans/planner-procedure-properties.json)
record their origin.

Parameterized Bind attempts keep separate checked planner values. Parameterless
Bind messages share their prepared statement's plan; each bound portal keeps
that plan alive. Bind retains decoded parameters through a preparation wait,
and cancellation releases the unfinished portal before another Bind can reuse
its slot. Planning can inspect already decoded parameters to skip a branch
such as `CASE WHEN $1=0 THEN checked_function() ELSE 17 END`, without running
the skipped function's constraints.

Planning checks expression types before evaluating constants. For example,
`SELECT min(active) FROM orders WHERE 1/0>0` rejects `min(boolean)` before
division by zero. A true constant `CASE` condition discards later branches.
The type-only pass releases its arena and temporary record descriptions while
preserving descriptions owned by the enclosing statement. SQL `EXECUTE` carries
declared parameter types through planning and execution, even for NULL values.
Metadata adapters carry declared sizes, collations and anonymous record shapes.
For example, `(q).f1` remains usable when `q` comes from `ROW(7)` in a nested
query. Expanded views retain their original schema and name while aliases
control which names SQL can reference. `RETURNING` adds the target's old/new
names without adding duplicate sources of unqualified columns.

Protocol preparation distinguishes starting a command from executing a block of
commands. A JDBC client can prepare and bind a standalone `COMMIT PREPARED`
without making it an explicit transaction. An earlier execution before `Sync`,
or an explicit `BEGIN`, still prevents that command with `25001`. Query analysis
pins the data snapshot; preparing a transaction-setting utility does not. Errors
roll back implicit work, while an explicit failed block remains failed until a
transaction-exit command such as `ROLLBACK`.
The connection retains its original command deadline between Parse, Bind,
Describe and Execute. Idle time does not renew that timer: the next supported
message checks it before producing buffered rows or replies, including Flush,
Close and Sync. An expired Sync still completes error recovery with
ReadyForQuery. COPY also rebinds its owning session, identity, search path,
rendering settings and original statement/transaction timestamps at start,
each row and completion. Another connection cannot lend it settings or reset
its clock. Once durable publication starts, timer restoration cannot cancel
the required commit receipt.
Standalone utilities with an immediate commit boundary, such as `CREATE DATABASE`,
complete durable publication before their Execute response. A storage wait resumes
that publication without executing the utility again. Prepared resolution retains
its dedicated completion path. Utilities that restart internal transactions need
their own publication boundaries; a blanket immediate commit is incorrect.

`DISCARD PLANS` makes future query admissions prepare again. Running calls and
bound portals or cursors retain their checked images; a compiled PL/pgSQL
function keeps its identity while its SQL statements can get new plans.
Ordinary PL statements without checked inputs or SQL planner candidates use
their enclosing function identity and reserve no checked-plan slot.
`DISCARD ALL` releases cached routine-input plans and their value owners.
Neither command re-evaluates inputs saved in schema definitions.
The [compatibility guide](postgresql-18-compatibility.md) records procedural
boundaries. Native regressions in `src/sql/tests/type_input_binding.rs` and
`src/sql/tests/type_input_positions.rs` retain the PostgreSQL 18.6 reference
SQL digests for these phase distinctions.

When an expression hands a mutable function to the engine, input preparation preserves completed
expressions, conversions, defaults and nested calls in the statement arena.
For example, `save_order(set_config('application_name',
current_setting('application_name') || '.', false))` appends one dot when the
input finishes across those function handoffs. Simple builtin trees that cannot
pause consume no retained input records. Engine-owned statistics, replication
and large-object calls use the same explicit handoff classification.
The pending call stores checked handles for its encoded arguments and selected
argument, parameter and result types. Those handles survive moving the owning
arena or recycling its temporary front; reset or a different arena rejects
them with an internal error before reading their bytes. Arguments become
immutable before publication. Execution reads their stored bytes directly;
even an older mutable handle cannot reopen that allocation for writing.
Callback lookup exposes the invocation log only inside a scoped closure. Its
installer keeps the restoring guard private and restores the previous owner on
success, errors or unwinding.
This owns the call's inputs. The running body and enclosing write still need
complete ownership across a scheduler return.
Completed scalar values and table-source rows keep their encoded bytes in
checked arena allocations too. Their result directory stores handles and effect
receipts, rather than references to returned values or row slices. For example,
a function returning `ROW('saved text')` keeps the field's name, type and text
after its body memory is reused. A body encodes its result in temporary memory;
as that workspace closes, the arena moves it directly into one immutable
retained image. This also works when the parent is another temporary arena.
The move handles overlapping bytes before writing the retained header, and
keeps no intermediate image in the parent's temporary front. Scalar consumers
copy their value into their own arena. Table queries with an explicit owner
rebuild row pointers that refer directly to the saved payload. A nested query
using the active callback copies its row image into its own arena before the
callback ends; no reference to the callback owner can escape. Ordinary table
queries avoid another full payload copy. Directory growth reserves its
replacement before publishing it; exhaustion keeps every previous result
readable. All reservations stay
inside the existing statement capacity and report `54000` if they do not fit.
The [nested-result reference](https://github.com/e6qu/pos3ql/blob/main/tests/external/reference/2026-11-06-nested-callback-results/README.md)
records summaries of 4,096-row results through nested SQL/procedural calls,
including exactly one sequence increment and audit write per invocation.
Unfinished input frames retain checked handles for values, call names, defaults,
type signatures, selected casts, dependencies and effect receipts. A new attempt
rebuilds temporary metadata views; an already encoded value is copied only when
the consumer reads it. Stale input selections return `40001` before choosing a
replacement function or cast. Publishing a suspension uses the arena's retained
tail, including while a row scan borrows its temporary front. The invocation log
and its sorted input directories store an arena identity and checked handles,
with no borrowed metadata or values. Moving the log together with its arena
preserves saved inputs, results and selected casts; resetting that arena rejects
the old log. Temporary execution scopes borrow both owners so neither can move
while a scan callback uses them. The result directory's published length is the
completed-call count, avoiding a separate counter that could disagree.
Nested SQL queries, including a `ts_rewrite` rule query, have their own input
scope. Their row scans may reclaim temporary storage without retaining a
caller's unfinished input frames in that storage. Returning from the nested
query restores the caller's input scope, including after an error.

Waiting for another connection's lock can require restarting an unfinished
function body. Completed cursor operations and checked routine inputs keep
their results and sequence effects. A caught input-check error keeps its exact
failed attempt too; replay restores that error without running the check again.
If restarting would lose their data writes
or session changes, the command fails explicitly with `0A000` and rolls back;
it does not silently repeat or discard those effects. Retaining the remaining
function-body execution position is still a roadmap item.

Numeric `FOR` loops reserve reusable local-variable frames before running a
call's local initializers. The reservation follows maximum simultaneous loop
nesting, rather than iteration count or the number of separate loops in the
source. Layout checks every branch and reserves only the largest local scope
needed at each depth; shadowed counters reuse their existing local slot.
Entering a loop copies its current scope into that depth's frame; leaving
copies back assignments while restoring the shadowed counter. Returning
the frame clears references into completed temporary work. Local descriptions
and value-slot arrays own ranges in the startup
execution control pool; `54000` names insufficient capacity. Variable-length
values in active local slots may still refer to statement memory.
This prevents repeated inner-loop entry from consuming fresh local buffers.
When a loop counter shadows a named output, the outer output value is saved as
typed bytes in a reusable, owned execution-control buffer. Nested counters keep the same
saved value; leaving the outer counter restores normal output lookup. For
example, `RETURN NEXT` inside `FOR answer IN 1..2` returns the function's text
output `answer`, while expressions in that loop see its integer counter.
Saved outputs retain record names/types, nulls and array bounds without borrowing
the expression's scratch. Growing a buffer reserves its replacement before
publication; failed sizing or reservation preserves the previous value.
Repeated entries reuse capacity based on the largest value, rather than the
number of assignments. Saving needs control-pool bytes and a range descriptor;
`54000` names `execution_control_bytes` and `execution_control_leases` on
exhaustion. Reading copies into an explicit destination arena and can instead
exhaust statement memory. A decoded value survives replacement or destruction
of its saved buffer. Saved bytes survive reuse or destruction of the source arena.
The [PostgreSQL reference](https://github.com/e6qu/pos3ql/blob/main/tests/external/reference/2026-11-06-saved-output-scopes/README.md)
records repeated and nested counters, caught errors and typed outputs.
Runtime local declarations contain names, resolved types and flags, without
references to initializer expressions. The compiled program owns those
expressions separately; initialization seeds parameters first, then evaluates
local defaults once in declaration order. For example, a later local can use
an earlier constant initialized by `nextval`, and a loop counter must restore
the initialized local that it shadows.
Procedural declarations and target names cross the SQL lexer before lookup.
Clause, control, assignment, range and argument boundaries use that lexer too.
For example, `INTO` in `EXECUTE $sql$INSERT INTO orders VALUES(1)$sql$` belongs
to the command string. Commas inside strings, array subscripts or comments do
not introduce `USING` arguments. Lexical errors retain their SQL error codes;
temporary token text is released before statement directories are allocated.
Unquoted `MixedCase` becomes `mixedcase`; quoted `"MixedCase"` remains distinct.
Local variables, record fields and built-in handler/status/context names compare
those normalized names exactly, so `SQLERRM` and `"SQLERRM"` are different.
Quoted cursor names use the same token boundaries for `OPEN`, `FETCH`, `MOVE`
and `CLOSE`, including spaces and doubled quotes. Local declarations can shadow
context variables: `DECLARE found integer := 42` makes both its value and
`pg_typeof(found)` use that integer. Inside an exception handler, its implicit
`SQLSTATE` and `SQLERRM` bindings shadow outer declarations. Values, declared
types and domain identities share one lookup rule, including captured cursor
inputs and detached conflict-update inputs. The
[PostgreSQL reference](https://github.com/e6qu/pos3ql/blob/main/tests/external/reference/2026-11-06-procedural-name-bindings/README.md)
records those cases. Quoted `"NEW"` is distinct from the trigger record `new`.
A numeric loop inside a handler can introduce its own integer `SQLSTATE`
counter; leaving that loop restores the handler's text error code. A handler
inside that loop introduces its own error code. Each binding records how
deeply its block or loop is nested. Reusing an error buffer does not change
which variable a name selects. Constant-target
validation follows the same nesting before the body runs, including `FETCH`
targets. Opening a constant cursor with an existing name reads that variable;
opening a NULL constant fails when a generated name would need assignment.
The [scope reference](https://github.com/e6qu/pos3ql/blob/main/tests/external/reference/2026-11-06-procedural-lexical-scopes/README.md)
records values, types and creation/runtime errors.
Function and procedure parameters occupy an outer scope. A declaration such as
`DECLARE value text := 'local'` can shadow an integer parameter named `value`;
reads and assignments then select the text local. Output parameters retain
their own slots, so changing a shadowing local does not change the returned
output. Parameters can also use the names `SQLSTATE` and `SQLERRM`; an error
handler temporarily introduces its own text variables. The
[parameter reference](https://github.com/e6qu/pos3ql/blob/main/tests/external/reference/2026-11-06-procedural-parameter-scopes/README.md)
checks scalar, set-returning and procedure outputs, types and cursor capture.
Trigger names and relation/schema names have PostgreSQL type `name`; `TG_RELID`
is an `oid`, and trigger argument counts are integers. `TG_TABLESPACE` is not
a PostgreSQL trigger variable and raises an undefined-column error.
Branches, loops and exception handlers use an explicit execution frame stack,
reserved to the maximum lexical nesting before local initialization. Each frame
stores a block index and next-statement offset in its compiled program. The
original body and program kind use an immutable source image owned by the
execution control pool. Compiled syntax and retained positions share that owner;
cloning it reserves no extra bytes or descriptors. Parsed
blocks belong to a separate compiled view. Mutable frame metadata owns a range
in the shared execution control pool and retains no references to syntax or
loop-value buffers. Output-binding metadata owns a separate range in that pool;
its saved values own separate byte ranges in the same pool. The same owner holds
the position and resource directories, so they cannot be rebound to a different
call's buffers. An entry owns its control data and required resource in one enum
before publishing them to their indexed slots. Numeric scopes own both local
descriptions and value-slot arrays in the control pool. Each call pairs its
resolved ordinary declarations, value slots and execution status together.
Procedural calls consume a row schema. Calls without row transitions use an
explicit empty schema, keeping unused table metadata off that path.
Initialization records whether the next variable needs expression evaluation or
conversion. Its evaluated value is saved before conversion can pause, and the
compiled source supplies the initializer expressions directly.
An omitted initializer still validates a domain's initial NULL before the body.
For example, declaring a variable of an integer domain marked `NOT NULL`
returns `23502` before a later `nextval()` can run. PostgreSQL 18.6 implements
that check at [block entry](https://github.com/postgres/postgres/blob/REL_18_6/src/pl/plpgsql/src/pl_exec.c#L1608-L1628).
A declaration explicitly marked `NOT NULL` needs an initializer; routine
definition validation rejects its omission with `22004`, as PostgreSQL's
[declaration grammar](https://github.com/postgres/postgres/blob/REL_18_6/src/pl/plpgsql/src/pl_gram.y#L507-L516) requires.
The internal interpreter also retains whether it is executing statements,
finishing a return or loop exit, or finding an error handler. Before rollback
cleanup can run SQL, an error takes its detail and hint out of the worker-local
diagnostic slot into the shared execution control pool. Cleanup cannot replace
those fields with another command's error. Pending return payloads join the saved row-work images
only when a call pauses. These internal owners still need the enclosing write
and scheduler handoff described below; they do not enable checked trigger inputs.
An error with no protected block to roll back returns its original diagnostic
directly, without reserving another image in the control pool.
Transition tables retain copied, interned column metadata
and declared type OIDs. For example, `pg_typeof(new_rows.amount)` keeps the
column's domain type when a statement trigger reads inserted rows, including NULL
values; it does not infer that type from the stored integer value.
Collection ends before a transition table becomes readable: publishing consumes
its writer, so later capture cannot change the published row list. Internal
saved transition images keep both lists in shared row-work memory. Restoration
admits both lists together and borrows their immutable payloads; even a large
row therefore needs only its list entry in resumption scratch. These images
still need the enclosing-write handoff below.
Mixed writes retain separate transition tables for inserts, updates and deletes.
Their row triggers drain in the original event order, with each trigger reading
its own operation's tables. For example, the insert trigger of an `ON CONFLICT`
command must not receive the rows changed by its update branch.
Parameter-slot arrays use that pool too; parameter staging reserves named
parameters only and releases its range after initialization. Numeric layout
scratch is released before entering the body. Taking a scope keeps its ranges
owned until that scope is returned or dropped.
Active value slots borrow variable-length payloads in statement memory. Internal
frame suspension saves ordinary and active numeric values as typed rows in the
shared row-work pool, then clears every borrowed reference while retaining the
original slot ranges. Only pausing captures the ordinary declarations into a
control-pool image; normal calls borrow compiled declarations, and repeated
pauses share the captured image. Restoration copies each saved row once and
decodes all its columns in one pass. Images are released only after every scope
has restored. A memory error preserves all images and reservations, including
when a child borrows the destination; the child's live bytes remain valid.
This frame owner alone does not yet retain the enclosing write or unfinished nested input work
through a scheduler return.
Table INSERT, UPDATE, DELETE and MERGE share the end of a write: drain its row events,
run the matching AFTER statement triggers in order, then send one completion or
return its affected-row count to the caller. INSERT with an update on conflict
runs UPDATE statement triggers before INSERT statement triggers. MERGE runs
AFTER statement triggers in DELETE, UPDATE, INSERT order, for its listed actions;
its BEFORE statement triggers run in INSERT, UPDATE, DELETE order. This follows
PostgreSQL's [write executor](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/executor/nodeModifyTable.c).
This common path does not yet retain the enclosing row loop through a scheduler return.
Row and statement triggers share construction of the selected routine and its
scoped settings, security and row metadata. A trigger cannot commit or roll back
its caller's transaction or inherit the enclosing command's SQL parameters.
These rules apply on every entry, including resumed calls.
Trigger execution combines the compiled program, lexical
frame, arguments and row binding in one firing owner. Saving a firing copies its
`OLD`/`NEW` payloads and arguments into shared row-work memory, retains the selected
column image and preserves transition aliases and declared type OIDs. Restored
transition rows borrow immutable saved bytes; modified `NEW` values are encoded
before temporary memory is released. Admission errors retain the previous owner,
and attachment checks connection, database, transaction and storage ownership.
Normal calls keep borrowing their row context. A configured routine's current
settings, original restoration baseline and effective security-definer user are
saved with the firing. Settings images use the existing execution-control pool;
no worker pointer survives suspension. Attachment checks the parent settings
before installing the image, and its guards restore the caller after another
pause, an error or completion. The enclosing write, remaining statement
environment and unfinished input work still need the scheduler integration
described above; this owner does not enable rejected checked inputs.
Restoration also lends the firing its original statement and transaction
timestamps and deadline. The transaction's consumed-timeout state prevents
rearming a timeout already handled by the routine. Returning from a poll restores
the worker's previous clock and timer, including after a failed attachment.
Saving a firing also records its position among the statement's completed
sequence calls and input checks, together with the sequence selected by
`lastval()`. The connection retains that record and its sequence session.
Attachment checks the original statement attempt and nested effect record before
advancing their positions. A failed attachment restores the caller's previous
position; another pause saves the new position without copying the record.
Advisory-lock calls have a separate fixed journal. A saved firing retains its
original statement identity and position in that connection's calls. Attachment
skips completed calls only after every firing resource is available; a failed
attachment restores the caller's position. New statements and reused connection
identities reject old positions with `40001`. Other connections can release
their journal slots without changing the retained call order.
An unfinished procedural instruction retains its own earlier position, after
input preparation. For example, if `PERFORM pg_advisory_lock(41),
pg_advisory_xact_lock(42)` waits on the second lock, resumption reuses the first
lock call's receipt rather than taking another hold. Sequence calls use the
same instruction boundary. Attachment validates both journals before publishing
either position. A completed instruction discards this boundary; the firing's
outer position continues to skip earlier instructions.
Write completion uses a cursor over queued row events followed by the selected
AFTER STATEMENT triggers. It retains the row-event index, trigger depth,
operation and last selected trigger name without syntax pointers. The same
selectors serve the immediate trigger boundaries, COPY and ordinary write
completion; nested trigger SQL cannot drain events from its caller's depth.
The cursor also owns the affected-row count and choice of client response or
internal count. It rejects completion before AFTER work ends and rejects a
second completion. A full output buffer removes the unfinished completion frame
and leaves the previous count intact; a retry can publish the new count and
complete message together. Saved firings also check their original command
identity and trigger depth before changing any execution context.
Mixed-write transition images keep INSERT, UPDATE and DELETE tables under one
owner. Restoration reserves all row directories in one allocation, so a failed
admission publishes none and cannot rewind an active child's memory. Payloads
stay in the existing row-work pool after the original collection arena drops;
failed capture releases any operation images already reserved.
Direct table writes without rewrite rules transfer their AFTER completion to
the engine after releasing the write's storage borrow. The retained owner keeps
the original undo boundary, physical definitions, transition images, remaining
trigger order and final response. A trigger that waits saves its active firing;
the frontend retains earlier `RETURNING` rows and resumes that owner without
redoing the insert, update or delete. Real errors use the original undo boundary.
The existing control arena reserves the completion state only for active writes.
A selected statement-trigger definition has its own temporary reservation,
released before that body starts; row-trigger selections retain only their
queue positions. Idle connections carry the small arena owner, not a maximum
trigger definition or an additional startup pool.
BEFORE and INSTEAD OF work, unfinished nested row phases, modifying CTEs, rewrite rules,
COPY and replication apply still need the same complete enclosing owner before
their rejected checked inputs can be enabled. Cold object-store waiting and the
full protocol paths remain qualification gates in the plan.
An internal nested-command owner also saves the command identity, nesting depth
and read visibility before restoring its caller. Attachment checks the original
connection, database, transaction and parent command before changing any state.
It resumes the same command rather than issuing a new identity. Completing or
pausing that scope restores the caller's visibility again.
Static and dynamic nested INSERT, UPDATE and DELETE transfer their unfinished AFTER work
into the calling firing. That firing owns the child's command scope and
completion together, resumes the child before advancing its own instruction,
and receives the affected-row count without emitting a second command tag.
Dynamic `EXECUTE ... INTO` saves its returned values, including record field names
and types, before a child trigger waits. Resumption assigns those saved values
without evaluating the command text or `USING` arguments again. Dynamic execution
updates `ROW_COUNT` while preserving `FOUND`; `STRICT` checks the completed row
count. The saved values use the existing row-work pool.
An `EXECUTE 'UPDATE orders ...' INTO result` needs `RETURNING` to supply values.
Without it, SQLSTATE `42601` follows the command and its triggers; a surrounding
exception block rolls back their writes, while issued sequence values remain
issued. Empty results keep their field names and declared types with NULL values.
`STRICT` errors preserve the target's earlier value and expose the completed row
count to diagnostics. These boundaries follow the
[PostgreSQL 18.6 executor](https://github.com/postgres/postgres/blob/REL_18_6/src/pl/plpgsql/src/pl_exec.c).
SQL record descriptions retain declared types separately from the protocol's
wire descriptions. For example, after `FETCH` from a cursor selecting
`NULL::positive_integer`, `pg_typeof(result.id)` still names that domain,
including when no row was fetched. Each cursor keeps only the selected type IDs
in its existing `cursor_query_bytes` arena; no extra per-cursor pool is reserved.
`SHOW`, `SHOW ALL` and `EXPLAIN` also supply named text columns to dynamic records.
An unfinished `PERFORM` keeps completed function results, their selected inputs
and receipt positions when the calling trigger pauses. For example,
`PERFORM write_audit('one'), wait_for_lock(), write_audit('two')` must insert each
audit row once even if the lock waits repeatedly. Only an actual pause copies
those typed values out of temporary query memory; ordinary completed queries
release that memory together. New calls after resumption use the same retained
owner while body evaluation uses separate temporary memory.
Unowned row phases still reject suspension with `0A000`; their full continuations
remain required before enabling those waits. The remaining expression paths,
unfinished function bodies and query positions also need complete ownership
before their temporary memory can be released across a scheduler return.
Nested procedural updates and deletes own independent row-selection ranges in
the shared row-work pool. They do not replace the caller's selection or borrow
it through a raw pointer. Inserts, merges, COPY and trigger dispatch do not carry
an unused row-selection workspace. For example, a trigger updating another table
must leave its caller's remaining target rows intact even when it catches an
error and rolls back a nested delete.
Binding a view checks the source owner and execution workspace before entering
the body. A different source image, even with identical text, or a foreign/reset
workspace returns `40001`. Source capture returns `54000` naming the execution
control settings on exhaustion. Rebuilding syntax copies the owned text into
explicit parsing memory; exhausting that destination names statement memory.
The source survives destruction of its input arena, parsing scratch and external
pool handles. Source images preserve text and program kind; the invocation's
selected definitions and settings require their own retained owners. An active
numeric loop can bind rebuilt syntax without resetting its range, local values,
shadowed counter or next statement, provided its value owner remains alive.
Loop conditions and handler syntax resolve through the bound view; loop values
stay in their paired directory and diagnostic/savepoint positions stay in the
control stack. Child blocks do
not recursively call the interpreter. Leaving a scope
restores shadowed counters from the unchanged parent slots, copies assignments
back and releases its references.
Caught errors own their handler directory and reusable byte buffers in the
execution control pool. The directory follows maximum simultaneous handler
nesting and is admitted before local initializers. Text capacity grows only
when an error is caught; repeated catches reuse that capacity. These owners
survive destruction of the arena that supplied the context text.
A handler reads through a borrowed view, without a copyable storage handle or
generation token. Rust prevents a live view from surviving buffer reuse or
owner destruction. Nested handlers keep the outer error available; leaving the
inner handler restores the outer view. SQLSTATE/SQLERRM and requested stacked
fields copy text before exposing it to local variables, so those copies survive
later catches and owner destruction. Pool exhaustion returns `54000` naming
`execution_control_bytes` and `execution_control_leases`; copying a field can
separately exhaust its destination statement memory. Failed capture leaves the
outer handler usable and does not enter the failed handler.
Assignment rewriting keeps the original target and value dependencies: for
example, `document[SQLERRM] := to_jsonb(SQLSTATE)` still sees both handler
variables after lowering to the shared JSON setter.
For example, saving the first `SQLERRM` in a 4,096-iteration error loop must
keep that message after later catches reuse the handler buffer. Missing stacked
fields return empty text. The [PostgreSQL reference](https://github.com/e6qu/pos3ql/blob/main/tests/external/reference/2026-11-06-caught-diagnostics/README.md)
records repeated catches, nested handlers and rethrow.
The complete invocation still needs the enclosing write, selected definitions,
settings and unfinished query/input work to survive returning to the connection
scheduler during a storage wait.
Materialized query and array loop inputs own their encoded values in
`row_work_bytes`/`row_work_leases`, shared with other foreground row work.
Source query work and nested invocation receipts use one scoped arena and are
released before the loop body. Each iteration decodes its selected value into
statement memory; record field names/types, nulls and array bounds survive.
Finishing a loop, leaving it early or raising an error returns its input lease.
Exhaustion names the shared work settings with `54000`.
Table-function lookup keeps its selected definition as a temporary value;
statement memory retains only actual SQL text and output names needed by parsed
queries or returned records. Repeated schema and execution probes do not retain
the definition's maximum-width argument, result and configuration arrays.
Functions producing record values in a result list decode the declared output
width into an actual-sized field slice; the input-argument limit does not size
that result buffer.
Set-function result collectors own encoded rows and actual output-column
metadata in that same shared pool. A row becomes visible to the collector only
after conversion, capacity admission and encoding finish. A failed append keeps
earlier rows, including a prefix returned before a caught SQL error; the next
append overwrites any unpublished bytes. `RETURN NEXT` releases expression and
conversion scratch after each row. Output-variable bindings reserve their actual
count before local initialization, rather than a maximum-width array for every
invocation. At function completion, exported rows receive independent statement
storage before the collector releases its leases. These owners do not yet make
the whole invocation resumable across connection-scheduler returns.
Input admission also releases parser/dependency scratch when no checked values
or planner candidates need retaining. Expressions and static statements with no
input substitutions reuse their compiled source while keeping the normal
input-owner scope. `PERFORM` uses a scoped arena for query work and nested
invocation receipts; both are released when the query finishes. Only its row
count and `FOUND` return to the interpreter. Prior locals, handler diagnostics
and input bindings stay outside that workspace.
It does not yet save a function's execution position across a storage wait.

Procedural exception scopes share savepoint admission, transaction-log rewind
and remote savepoint mirroring with explicit SQL savepoints. Every admission,
release and rollback refreshes the savepoint list used by a newly opened foreign
connection. A connection first used inside a failed block must undo that block's
remote writes too. Engine-owned
scopes also undo catalog changes and staged journal records. Cursor creation
marks preserve older cursor positions while releasing new, unfinished and failed
cursor openings. Atomic row-trigger scopes have no catalog or journal owner;
their supported row changes, constraints, settings and deferred events rewind
through the same transaction-state marks. This cleanup is separate from the
unfinished retained execution needed to keep work through a private wait.

Internal waits and routine handoffs carry a typed execution marker, separate
from their diagnostic code. Parsing a SQLSTATE from SQL or a remote error never
creates that marker. For example, an application raising `PZ001` returns or
catches its error normally; it cannot park a connection. Exception handlers
exclude actual engine controls, rather than reserving application error codes.
Domain CHECK evaluation also preserves whether an error came from running the
expression or from a false result. Input-validation functions report the latter
as invalid data; they propagate expression errors, cancellation, exhaustion and
engine handoffs. Copying an error preserves that origin, while SQL handlers
still compare its ordinary five-character code.

SQL `SET` parses SQL syntax; `set_config` and saved settings take decoded values.
For example, `set_config('search_path', 'orders,public', false)` preserves that
spelling, while `SET search_path TO orders, public` stores a formatted SQL list.
Both validate the list before lookup. Routine scopes and transaction rollback
restore the stored value without parsing it again as SQL.
Leaving a configured routine restores only settings named in its `SET` clause.
For example, a trigger configured with `SET application_name = 'audit'` restores
the caller's application name, but an unlisted `SET LOCAL bytea_output = 'escape'`
remains in effect until transaction end. `current_setting()`, `pg_settings` and
binary rendering see the same value. An explicit session-wide `SET` overrides
routine restoration even when it assigns the previous value; subsequent local
changes remain until commit. Fixed per-setting action stamps distinguish that
case from an unchanged value, and rollback restores those stamps with settings.
Their connection-wide counter never wraps: exhausting its 64-bit range returns
`54000` with `session setting assignment counter exhausted` before changing a value.
These rules follow PostgreSQL's [SET contract](https://www.postgresql.org/docs/18/sql-set.html)
and [setting-stack implementation](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/utils/misc/guc.c).

Scalar functions, table functions and procedures share input binding, while
keeping PostgreSQL's distinct null and result rules. Typed structural copies
preserve named and anonymous row fields through SQL and PL/pgSQL functions,
triggers, pending calls and the binary protocol. A named row is one value in
`SELECT make_order(1)`, but its declared fields become columns in
`SELECT * FROM make_order(1)`. A single-column `RETURNS TABLE` produces its
column value in the SELECT list; multiple columns produce a record. Result
descriptions use declared types and never sample an argument's value.
See [compatibility](postgresql-18-compatibility.md) for record, conversion and
capacity limits and the regression suites for individual cases.

Single-table statements with supported scalar expressions reuse the owned
physical reader across mutable function handoffs. Execution keeps the current
row, finishes its predicate once and retains each completed projection item
before advancing to the next. Function targets, original defaults, selected
conversions and whether null inputs skip a call bind before row effects.
Replacing a function can change its body when it starts, while the current
query keeps that null-handling
decision. A session-setting change in an earlier column cannot redirect
a later bound call. Each active row borrows the unused region of its existing
statement arena; completed output is copied before that region is released.
Nested function execution can borrow a further region without growing memory.
Row ownership and scan positions survive outside those temporary regions.
Completed function bodies also release their parsed statements, local values
and temporary plans. Only encoded results move into the caller's arena; this
applies to scalar functions, table functions and engine-owned calls, including
nested calls within a row. For example, a function can create and discard 64 KiB
of text for each of 1,100 input rows without retaining all that text. Live outer
locals and results still need space, and a single oversized call fails with
`54000` rather than growing memory.
Issued sequence numbers keep command-lived records in the owning arena. For
example, a number issued before a lock wait must not be issued again when that
command retries. Temporary row regions and the command's retained records share
a checked allocation frontier; recording an effect reserves space before it
runs, and ending a row preserves those command records.

This does not complete every query continuation. Complex plans, functions with
subqueries or set expansion, named/custom casts and function bodies still have
restart paths. The cursor reader retains simple table scans, constant derived
VALUES rows and one-row queries without a table source, including scalar function
calls and built-in casts. A typed syntax marker distinguishes comma-separated
VALUES from explicit set operations when selecting PostgreSQL's scroll behavior.
Other cursor plans still materialize their results when opened. Cursors and results already sent to the
client need the same once-only execution positions before all storage waits can
yield. Exhaustion remains a named error; no extra per-table or per-query backing
allocation is introduced by the scoped row region. [PLAN.md](../PLAN.md) owns the
remaining acceptance and deployment qualification.

## Reads and storage layout

The durable layout is log structured: new transactions and saved generations
append immutable objects. Small mutable roots identify the committed log and
checkpoint inventory. An object with a content-derived name is never edited in
place. Local RAM and disk accelerate access and can be rebuilt from durable
state in object-store mode.

| Mechanism | What it saves | Example and limit |
|---|---|---|
| Full-slice column groups, called PAX | Downloads for columns that are not needed | Reading `id, status` can skip a large description column; predicates, reconstruction and exact checks can still require more columns |
| Packed, compressed row deltas | Descriptor and column-object overhead for small changes | Updating a few orders writes small row groups rather than a full wide-table slice |
| Eight-byte column extent alignment | Gives encoded column values aligned offsets within packed objects | This is a layout guarantee, not a CPU throughput or sector-size claim |
| Packed objects and verified byte ranges | Request overhead and unnecessary transferred bytes | Several independently verifiable extents can share an object; a ranged GET fetches the selected extent |
| Sparse row indexes and Bloom filters | Reading generations or groups that cannot contain a row | An absent order number can avoid a data read; several generations can still need checking |
| Durable value indexes | Searching unrelated rows or regions | Btree/hash, geometric bounds, interval bounds and GIN postings narrow candidates; exact SQL and visibility checks still determine results |
| Reuse of unchanged immutable groups | Rewriting already saved data | Checkpoint/merge work can reference unchanged PAX groups; changed groups still need output |
| Memory/disk caches and retained reader buffers | Repeated downloads and repeated decoding after a wait | A resumed query retains completed columns and navigation even if the shared cache evicted them |
| Bounded batches of object requests | Serial network round trips | Missing selected columns and independent physical writes can overlap within configured request slots |

These mechanisms are implemented; their production benefit has not been
quantified. PAX is a column-group layout within saved row generations. It does
not imply a separate user-selectable columnar table access method, vectorized
SQL execution, multi-core queries or Parquet compatibility.

An immutable sorted row file, or SST, orders versions by row identity and
commit position. Newer versions are considered before older versions for the
same row. A reader combines applicable saved generations with its resident
changes and deletion markers. Large rows can require an overflow chain;
reading one row is therefore not a fixed number of GET requests.

See [index navigation](index-navigation.md) for searchable summaries and exact
checks, and [durable formats](durable-format.md) for encodings and accepted
readers. PostgreSQL's catalog alignment and heap properties describe the SQL
interface; they do not prescribe pos3ql's object layout.

## Writes and the success response

1. SQL checks permissions, constraints and locks and prepares private row and
   catalog changes. Rollback records and journal staging must fit their pools.
2. Before durability, admission reserves replacement table/type definitions,
   index-binding roots and row keys. It retains the completed command while
   required locks or storage are unavailable.
3. A commit captures immutable journal bytes and a descriptor. Independent
   uploads can overlap, and several transactions can share the publication.
4. A conditional update advances the commit head only after the required
   uploads succeed. It preserves the committed predecessor and writer identity.
5. The complete transaction installs in memory, its visible position advances,
   locks release, and the server can send success.

**Example:** updating an order and its invoice in one transaction cannot expose
one changed value and one unchanged value during installation. An older report
can retain its earlier values. A new command waits through the installation
boundary rather than observing half a transaction.

Admission is still cancellable before durable publication. A wait preserves
completed defaults, triggers and buffered results; resuming it continues the
commit instead of executing the command again. Share relation locks protect
affected tables: ordinary SELECTs and unrelated work can proceed, while
conflicting writes wait. A prepared transaction captures its lock image after
admission has acquired those locks. Publication consumes reserved owners rather
than allocating or rebuilding metadata after durability.

After a restart, saved transactions reconstruct their private row and catalog
changes from the journal, including types, indexes and stored queries. Other
transactions still see the committed definitions. Once all startup catalogs
exist, stored-query references are bound separately for committed definitions
and each saved transaction's selected private definitions, in its own database.
For example, a saved schema rename keeps the original function usable until
commit, then its body reads the renamed table. `COMMIT PREPARED` completes
admission before making its decision durable, then installs these reserved
changes through normal publication. It does not replay allocating catalog
operations after confirming the decision. `ROLLBACK PREPARED` uses the same
captured identities and ordinary undo records to release them.

The commit head is ordered because confirmed transactions need one recoverable
history. This does not require serial uploads or serial future CPU execution.
Parallel CPU execution remains unfinished; concurrent object I/O is already
implemented. More upload slots do not remove root-update latency or conflicts
between transactions updating the same row.

With `object_store = off`, success instead requires local journal durability.
That mode cannot recover lost local-only data from an object store. With
object storage on, a lost success reply is an uncertain client outcome, not
permission to repeat an external payment blindly.

## Maintenance and scheduling

The server advances checkpointing, file merges, history cleanup, heap relocation
and eligible deletion in bounded turns. It runs between client messages and
while idle. Socket readiness resumes blocked network requests; owner contention
and failed maintenance use the existing one-second retry cadence. A quota of
64 visited row slots is a work bound, not a latency guarantee for a turn.

Row-location capture and relocation preflight visit the ordinary shared row
pool once, then the separate large-object pool. History cleanup also visits the
ordinary pool once and uses the large-object owner's cursor for its separate
reserve. It does not scan every shared slot again for each configured table.
Targeted cleanup follows only that table's owned links. Its typed cursor reports
a current position, a skipped stale position or completion; skipped positions
still count toward the work limit. Generation checks prevent a reused cell from
being treated as the earlier row.

A checkpoint writes captured committed versions and publishes a complete
manifest, the inventory of saved objects and definitions. Later commits remain
in the journal. Finishing an older checkpoint cannot clear newer writes or
ANALYZE results. Index output is eligible only when both its catalog identity
and its row-decoding context are committed; a committed index name alone cannot
make a private rewritten table shape safe to save. Merges combine saved
generations, retaining the versions needed by readers. Object deletion respects
current roots, retained readers and backups.
There is no need for an operating-system cron entry to drive this internal work.

`REINDEX` rebuilds from rows using the selected table shape and the existing
cache's transaction ownership. It does not turn a committed cache into a private
one or replace another transaction's cache. Rebuilding from rows also avoids
depending on the damaged saved index that the command is repairing.

For example, `REINDEX TABLE CONCURRENTLY orders` creates replacement indexes
while normal reads and writes continue. It first commits their catalog entries,
waits for earlier writers, builds each index, then validates it against later
writes and waits for earlier readers. It swaps names and constraint links,
retires the old indexes, and commits their removal before reporting completion.
The table's primary-key constraint keeps its identifier and comment; its
backing index receives a new identifier. Index registration leaves the table
shape intact so an existing writer cannot block publication of the replacement.
Inline keys are normalized when their backing index is swapped. An inline key
and its stored physical index produce one catalog row, joined by the index's
creation identity. That row keeps the constraint's primary-key flag and the
index's current placement and build flags. Index comments
move through the journal with the replacement and survive restart; reusing a
retired name cannot move another index's comment. The virtual large-value index follows
the same lifetime rules without introducing a separate PostgreSQL chunk heap.
Schema and database rebuilds finish one table before moving to the next.
Their end hooks run once after the complete command. If another connection
renames an earlier index while a later table waits, the hook reports its current
name. If that connection drops the index, the hook omits it. An index that later
reuses its catalog slot does not appear in that earlier event.

Keys retain the name chosen at creation. For example, renaming `orders` to
`archived_orders` preserves `orders_pkey`; rebuilding it also preserves that
name. The saved name follows the column's stable number through column removal
and recovery. The shared index description includes each key's comparison rules,
so rebuilding a primary or unique key writes the same rules to the journal.
Incomplete index records fail before changing the journal buffer.
`ALTER INDEX orders_pkey RENAME TO archived_orders_pkey` works before any
rebuild. It stores a physical index description when needed, preserving the
index and constraint identifiers, comments and rollback behavior. Ownership is
checked before reserving that description or taking the maintenance lock.
Renaming an inline key recognizes its own stored index by both table and index
identity. A different relation with that name still raises `42P07`.
Changing a table or materialized view's owner also changes its
live indexes' owner in the same transaction, including replacement and large-value
indexes. Rolling back restores those owners together. An old index that the
rebuild has stopped maintaining (`indislive=false`) keeps its former owner until
it is dropped.
Catalog rows report each stored index's selected owner, including retired
indexes and the separate indexes used for large values.

These waits retain their source, selected table lifetime and bounded metadata
leases across commits. Cancellation can leave a committed invalid replacement
or a retired index, as in PostgreSQL; rollback cannot erase an earlier committed
phase. Temporary indexes rebuild in place. Direct concurrent rebuilding of an
exclusion index is rejected; a table rebuild warns and skips it. A table rebuild
also warns and skips live invalid indexes. An explicit user-index rebuild can
repair one; retired indexes stay untouched by table-wide maintenance. See the
[compatibility guide](postgresql-18-compatibility.md)
for supported boundaries and [durable formats](durable-format.md) for recovery.

Application jobs, migration scheduling and backup schedules are operator work.
There is no built-in SQL job scheduler or PostgreSQL background-worker ABI.
See [operations](operations.md#scheduled-work-and-schema-migrations) for examples
and [backup rules](backup-restore.md) before scheduling an offline backup command.

## Memory and physical size

`row_overlay_slots` budgets resident row metadata across ordinary tables in all
databases. It does not multiply by `max_tables`. The internal large-object table
keeps its separate `large_object_pages` allowance. Empty table identities and
other catalog structures still cost memory.

`row_work_bytes` is one foreground byte pool for retained row-ID lists and DML
buffers; `row_work_leases` bounds concurrent owners of that space. The byte
capacity must be a multiple of 64. Each allocation rounds up to 64-byte alignment,
using at most 63 alignment-padding bytes per live lease. Exact read captures
reserve their known row count. Active write buffers grow geometrically within
the hard pool limit, so their spare capacity also consumes this budget. If a
growing buffer cannot extend in place, both old and new ranges and both lease
descriptors remain charged until its data has been copied. Fragmented free
space or this temporary overlap can prevent a larger contiguous request. The
named error reports requested bytes, total free bytes, the largest free extent
and remaining lease descriptors. For example,
an update and a long report compete for this same budget, rather than each
reserving a maximum array for every potential table.

`execution_control_bytes` reserves shared memory for procedural positions,
resource directories, resolved local declarations, local-value slots, source images, named-output bindings,
saved output bytes and caught error directories and text.
`execution_control_leases` bounds simultaneously owned ranges. A call reserves
one immutable byte range for its actual source text, shared by compiled syntax
and retained positions. It also reserves two ranges for its position and
resource directories, sized to maximum lexical nesting, plus a range for
nonempty local-value arrays. Pausing also captures actual ordinary declarations;
resuming and pausing again share that image. Set-returning
functions with named outputs also reserve their output-binding range. Numeric
loops reserve a scope directory and two ranges per maximum active nesting level:
local descriptions and value slots. Calls with exception handlers reserve one
handler-directory range, sized to maximum simultaneous handler nesting. Named
parameters may own another value range during initialization; it is released afterward. Empty tables and cached plans
receive no private execution ranges. These metadata and slot ranges are admitted
before initializers; partial admission returns ranges already reserved. Saved
output and caught-error buffers acquire a descriptor and grow as values are
captured. A full pool returns `54000` naming both settings. An error while saving an output or catching
a diagnostic follows the enclosing exception and transaction rules; an uncaught
error rolls back the command. The byte limit
must be a positive multiple of 64; startup accounting includes its backing and
lease/free-range records. Both settings are explicit in configuration files.
Active local-value payloads retain separate statement or foreground-work ownership; this pool alone does not resume a whole invocation.

For example, a function with 128 declared locals and one named input stages one
parameter value, then releases that staging buffer after initialization. It does
not reserve a second 129-value array to hold that single input. Its local slots
stay owned for the call, while larger text or record payloads still live elsewhere.

`table_metadata_bytes` reserves one shared backing pool for actual-length table
columns, physical column-number records, constraints, pending edit mappings and
index-binding collections, plus actual bytes of saved checked SQL inputs.
SQL sources without such inputs reserve no input payload. Its starter value is 64 MiB;
`table_metadata_leases` starts at 8,192 concurrently occupied ranges. File
configurations must name both settings. Bytes must be a positive multiple of 64;
allocator controls and per-table headers are additional startup reservations.
Prepared value-index keys also use one range per distinct row alongside schema
and binding ranges. Thus 8,192 leases is not an 8,192-row transaction limit:
bulk changes need room for their keys and the other live metadata. Exhaustion
fails before the journal becomes durable and rolls the changes back.
Each range rounds to 64 bytes. Fragmentation, retained old versions and the
overlap with replacement data can exhaust bytes or descriptors independently.
The named `54000` diagnostic identifies the metadata pool and requested, free
and largest contiguous bytes plus available descriptors. Pools never grow.

Each connection reserves `prepared_bytes` for every SQL and protocol prepared
statement slot, plus one `prepared_bytes` buffer for active input preparation.
For example, with `max_prepared=8`, those buffers reserve `17 * prepared_bytes`.
Text, parameter declarations and checked literal values share each buffer.
Exhaustion returns `54000`; increasing the setting takes effect at startup.

`routine_input_slots` reserves plan-control records per backend; the starter
configuration uses 128. Encoded values lease only their actual bytes from the
global `table_metadata_bytes` pool and consume its lease descriptors. There is
no maximum-sized value buffer per plan or per table. Nested and backend-static
plans share the fixed control capacity. Exhaustion returns `54000` naming
`routine_input_slots` or the metadata pool; neither grows at runtime.
The same input-slot limit bounds each backend's watched plans. A watch shares
the plan's actual dependency array; it does not reserve a list for every table.
The transaction's fixed DDL log retains matching invalidation receipts, so
commit and rollback use the selected object even if a name has changed or a
catalog slot is later reused. Other backends receive committed changes; an
aborted edit affects only its originating backend's cache.
The existing transaction TRUNCATE log also retains each affected table's creation
identity. These actual-length records share `table_metadata_bytes` and its lease
limit; no table-count-sized identity array is reserved per connection. The
transaction reserves only `max_ddl_per_transaction` event controls and its
existing table-name encoding buffer. Pool exhaustion returns `54000` before
publishing an event. No catalog lookup is needed after a table's definition has
been released.
Index renames preserve saved inputs. Whole-relation catalog statistics changes,
index creation, index deletion and TRUNCATE refresh plans that depend on the
affected table. Repeating ANALYZE without changing those catalog fields does
not itself require a new input check.
A compiled procedural function tracks its own definition. Each reached SQL
statement separately tracks its schemas, role-sensitive dependencies and
selected support functions, including custom operators and casts.

`statistics_bytes` and `statistics_leases` reserve a separate shared pool for
analyzed-column records. The starter configuration uses 1 MiB and 1,024 ranges.
Each immutable image stores only its analyzed columns, identified by their
original PostgreSQL column numbers. Readers, pending changes and checkpoints
retain selected images until they release them. `ANALYZE orders(total)` merges
that column's new statistics with the other columns' existing records. It
reserves the replacement before changing visible state. DDL removes statistics
for dropped or altered columns with the same admission and rollback rules.
Retained images and replacement overlap can return named `54000` exhaustion;
commit and rollback use the already admitted ownership.

`index_statistics_slots` independently bounds index activity counters; the
starter configuration uses 4,096 slots. Index creation reserves a counter
before catalog publication. Readers pin the selected index's qualified counter,
so a dropped index cannot redirect a late update to a replacement object.
Counter reset preserves catalog admission. A retired counter becomes reusable
after its readers release it. A table without indexes owns no such counter.

SERIAL defaults and identity columns use the shared sequence catalog rather
than another maximum-width counter array for every table. Sequence ownership
and the default expression are distinct: `OWNED BY NONE` removes the ownership
dependency while the column can continue calling that sequence.

A one-column table stores its actual columns instead of a maximum-width array.
Dropped columns still retain their PostgreSQL column numbers. Committed schemas,
pending edits and retained readers share immutable ownership; cloning a reader's
schema pin neither copies its payload nor reserves another range. Old and new
metadata remain charged until their last owners release them. Rollback and
prepared recovery preserve the selected object lifetime, including instance, database,
creation identity and schema revision; reusing a name or slot cannot redirect it.

`definition_image_slots` bounds retained execution-schema identities. Physical
images pin stored definitions; synthetic catalog images retain actual columns,
including their hidden suffix, in a separate typed payload. Image identifiers
include their owning pool and generation, so a stale or foreign identifier cannot
select another schema. CTEs, table functions
and trigger descriptions use borrowed column descriptors and actual statement
memory. They do not put owning schema leases or maximum-width definitions in
arenas that do not run destructors. Joined-row decoding also reserves only the
actual physical fields of each source. Recursive queries retain their internal
`SEARCH` ordering and `CYCLE` tracking fields in that physical shape while
exposing only the SQL-visible column prefix where required. Hiding a field
does not remove the scratch space needed to decode it. Checkpoint and relocation
reserves remain separate so foreground work cannot consume their progress capacity.

MVCC row history uses the explicit shared `row_history_slots` capacity across all tables and large
objects; `max_row_versions_per_row` remains a separate per-row bound. Locking
mode reserves no committed-row history. An update owns its prospective history
slot with its pending row chain, before durability. Queued and prepared commits
keep that ownership; commit links the reserved slot without allocating another,
and rollback releases it. History exhaustion returns `54000` at the write boundary.
Obsolete versions can be reclaimed while newer readers are active. On slot
pressure, write admission examines at most 64 row metadata positions and retries
with its owned input. Each history chain has the separate per-row version bound.
It never repeats SQL expressions or drains the whole
database synchronously; remaining pressure returns the named capacity error.
Foreign-table/catalog records and other fixed controls still add per-table
startup cost. Pending statistics and other definition families retain bounded
control slots, while column payloads share the independent statistics pool.
Row bytes, committed history and immutable
block caches also use shared startup pools. The
[adaptive resource design](adaptive-resources.md) separates implemented hard
capacities from future workload policies; no controller resizes startup backing
according to load. Catalog capacities remain independent: changing `max_tables`
does not silently resize indexes, routines, views or extended statistics.

The [embedded memory helper](https://github.com/e6qu/pos3ql/blob/main/examples/embedded-memory.rs) and
[runner script](https://github.com/e6qu/pos3ql/blob/main/tools/measure-embedded-capacity.py) measure 1, 8, 32 and 128
configured tables with the same explicit shared capacities in each transaction
mode, including table metadata, statistics backing and index-counter capacity. Its 64 samples
cover empty, hot, even, moving-demand, 64-column and schema/index replacement
workloads, plus narrow/wide statistics collection and targeted statistics
replacement through a type change and savepoint rollback. The single session
does not measure pinned-reader pressure; separate
ownership regressions cover that behavior. The helper retains named startup draws, configured reservation, peak resident
memory, stack size and startup time. Populated workloads are reported separately
from empty engines and include their session costs. These results must be
collected on CI; the script and configuration alone are not measurements.
The statistics pool is fixed independently of table count. Older samples used
implicit table-dependent sizing and retain that provenance.
The retained [empty-engine baseline](https://github.com/e6qu/pos3ql/blob/6ff1ac865a86a5318cb4934fbf01be0926667811/benchmarks/baselines/2026-10-30-embedded-startup/README.md)
includes raw measurements and their revision; its resident costs must not be
mistaken for configured byte reservations or a production minimum.
The helper uses the same explicit 128 MiB virtual stack reservation as the
server and native tests; ordinary operating-system thread defaults are not the
current execution contract. Peak resident memory includes the pages actually
touched. The debug-build helper records this stack size with each sample.
It also records each successful named startup reservation, so a large fixed
cost can be traced to its owning pool rather than inferred from total memory.
The retained [component breakdown](https://github.com/e6qu/pos3ql/blob/6ff1ac865a86a5318cb4934fbf01be0926667811/benchmarks/baselines/2026-10-30-embedded-components/README.md)
shows substantial fixed and per-table definition reservations in that revision;
reserved bytes alone do not establish which pages became resident.
The [before/after measurements](https://github.com/e6qu/pos3ql/blob/4054a6d3c79e916728f8e84d84139eae6d86b501/benchmarks/baselines/2026-10-30-embedded-lazy-definitions/README.md)
show empty-engine peak memory falling from about 735 MiB to 431 MiB with 32
configured table slots after unused pending definition payloads were left
uninitialized. Those historical debug-build samples do not measure the current
shared-schema implementation or establish a production memory minimum.

Capacity is the smallest applicable bound, not just the size of the bucket:

| Boundary | Current bound and practical meaning |
|---|---|
| Encoded immutable block | At most 262,144 bytes including its 52-byte header; payload at most 262,092 bytes |
| Saved row overflow | An inline head plus at most 16 overflow blocks; assembled encoded row at most 4,455,030 bytes. Input, statement and result buffers can impose smaller bounds |
| External execution-run row | At most 262,072 encoded bytes without overflow chaining, before other statement-memory limits |
| One row-generation roster | At most 8,190 referenced physical blocks, excluding the roster itself; indexes, filters, descriptors and overflow consume entries too |
| Index navigation within a generation | At most 4,096 index leaves; leaf and root addressability do not override the smaller physical-block roster |
| Resident row heap | `memtable_bytes` at most 4,294,967,295 bytes per buffer, with two buffers reserved; these are cache limits, not total durable table size |
| Table catalogs | `max_tables` at most 10,000 user table slots across the instance, plus the internal large-object relation; materialized-view backing tables consume slots |
| Global saved-block inventory | `checkpoint_live_blocks` must cover the distinct reachable block identities, including retained readers and backup data |
| Additional startup bounds | Manifest bytes, replayed commit batches, generations per table, merge/write entries, locks, transaction changes, connections and statement memory can fill first |

The 8,190-block roster gives a gross upper bound of about 2 GiB of framed
referenced block bytes for one generation if all blocks are full. It does not
promise a 2 GiB user table: metadata consumes entries, blocks can be short,
compression changes logical size, and a table can use several generations.

**Sizing example:** `checkpoint_live_blocks = 65536`, as in the development
configuration, corresponds to at most 16 GiB of full 256 KiB referenced block
bytes in that inventory. Small packed objects, metadata, indexes, historical
versions and backups reduce the useful row-data share. It is not a demonstrated
16 GiB database capacity or a limit that the object-store provider imposes.
Increasing a reservation requires a new startup plan and memory; it never grows
automatically. A larger bucket alone does not fix an exhausted inventory.

`row_overlay_slots` limits resident row metadata, not all saved rows. Saved rows
stream from immutable generations. Restoring a checkpoint installs their saved
file inventory without loading every row into the pool. Newer journal changes
still need resident slots and row bytes while startup replays them; a large
uncheckpointed tail can exhaust those reserves. Uncommitted changes and needed
history also consume their own resident capacity. A single large transaction can exhaust memory
while the durable table is small. Named exhaustion usually returns `54000`;
PostgreSQL-specific width errors retain their own codes. The
[capacity inventory](postgresql-18-compatibility.md#capacity-boundaries) owns
exact SQL widths and per-workspace reservations.

There is no qualified maximum database size, bandwidth, statements per second
or transactions per second for an independently operated deployment. The
[performance guide](performance.md#estimating-bandwidth-and-throughput) gives
explicit calculations and dated measurements without turning them into a
production guarantee.

## Schema migrations and binary changes

Application schema migrations use supported SQL through a normal client.
Changes such as ADD COLUMN, CREATE INDEX and type rewrites have their ordinary
locks, memory requirements and validation costs. Transactional DDL and savepoints
do not make a migration nonblocking or make an arbitrarily large rewrite fit.
A migration tool must coordinate which client applies each version; pos3ql does
not supply a migration runner or migration-history table.

Changing the pos3ql binary is separate. Earlier pos3ql format compatibility is
not required and there is no automatic format migration. Read the
[format matrix](durable-format.md), retain a recoverable source and verify a
logical export/restore when formats differ. A same-prefix backup does not
convert its bytes to a new format. PostgreSQL physical backup/WAL tools cannot
read pos3ql objects.

## Source map and outstanding work

| Source | Responsibility |
|---|---|
| `src/server.rs`, `src/pg/` | Connections, wire protocol, readiness events, retained waits and response order |
| `src/sql/` | Parsing, type checks, execution, transaction coordination and PostgreSQL catalogs |
| `src/storage/` | Resident versions, locks, retained definitions, object lifetimes and fixed metadata pools |
| `src/wal/` | Journal record framing, checksums, staging and recovery codec |
| `src/checkpoint.rs` | Saved generations, merges, manifest publication, retention and cold recovery |
| `src/store/` | Block formats, compression, immutable indexes, ranged access and cache tiers |
| `src/object_store.rs`, `src/object_store/` | Provider-neutral outcomes, S3 HTTP/TLS, signing and conditional operations |
| `src/mem/`, `src/config.rs` | Startup sizing, fixed buffers/pools and allocation enforcement |

`src/storage/catalog_identities.rs` checks durable catalog creation stamps and
counter reservations. `default_acls.rs` keeps a default-permission group's
identity separate from its recipients; `partition_triggers.rs` stores inherited
trigger identities and modes in the existing startup-reserved matrix. SQL records
private changes for rollback and publishes their final expected/replacement
images at commit. Checkpoint and journal readers restore saved positions and
reject references to replacement objects. Database copies rebind parent links
within the new database before installing inherited triggers.

Journal cleanup uses a separate fixed metadata client and retains its listing,
backup-retention and deletion positions across socket waits. It can run while
foreground block reads and journal uploads wait. It has its own fixed backup
list and deletion queue, so checkpoints can publish and free row-cache space
while obsolete-log requests wait. A pass has a fixed cutoff; newer checkpoints
queue the latest next cutoff instead of restarting the active pass.
Saved-file cleanup uses a second fixed client and retains its backup, manifest,
row-roster, index-tree and deletion positions across waits. Its cache probes
preserve query read owners. It validates the complete keep-set before deleting
files, then continues each listing from its saved cursor. Durable file writers
remain paused while that decision drains; foreground RAM edits can continue.
Row-file merges retain index descent, source-row scheduling, column downloads
and large-row fragments while a request waits. Scheduling reads row descriptors
without fetching PAX column values. Completed groups stay in the existing source
buffers; checksums and output positions advance only after their rows finish.
The fixed read pool reserves one maintenance owner alongside connections and
subscription workers. Checkpoint row capture, row output, index input scans,
sort-run merges and final index output use the same maintenance owner. Each
retains its exact next input; a waiting job resumes before another job can
reuse that owner's response. Private startup-reserved row and scan buffers keep
partially read values separate from foreground queries. An emitted index row
is consumed once even if fetching its successor has to wait.

For example, while an index on saved `orders(customer_id)` waits for storage,
a cached order lookup and another connection's durable commit can finish.
The index becomes visible as a saved root only after its complete output uploads
and the matching manifest publishes. Replacing its table or changing its inputs
abandons the old job and releases its read request.

The remaining order is coherent shared row/catalog publication and bounded
maintenance, startup-reserved CPU workers, then representative performance and
multi-host recovery/failover qualification. Current [behavior](implemented-baseline.md),
[known limits](postgresql-18-compatibility.md), [operations](operations.md) and
[the plan](../PLAN.md) each own one part of that contract.
