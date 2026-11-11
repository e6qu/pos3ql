use super::*;

/// PostgreSQL's special date/time inputs (`now`, `today`, `tomorrow`,
/// `yesterday`, `epoch`, `allballs`) read the transaction clock in the session
/// time zone.
#[test]
fn special_datetime_inputs_follow_the_transaction_clock() {
    let (mut engine, mut budget) = test_engine();
    let output = run_with(
        &mut engine,
        &mut budget,
        "SET timezone = 'America/New_York'; BEGIN; \
         SELECT 'now'::date = current_date, 'today'::date = current_date, \
                'tomorrow'::date - 'yesterday'::date, 'now'::timestamptz = now(), \
                'today'::timestamptz = date_trunc('day', now()), 'now'::timestamp = localtimestamp, \
                'tomorrow'::timestamp - 'today'::timestamp, 'now'::timetz = current_time; \
         SELECT 'epoch'::timestamp, 'epoch'::timestamptz, 'epoch'::date, 'allballs'::time; \
         COMMIT",
    );
    assert!(
        error_sqlstates(&output).is_empty(),
        "{}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(
        data_rows(&output),
        [
            "t|t|2|t|t|t|1 day|t",
            "1970-01-01 00:00:00|1969-12-31 19:00:00-05|1970-01-01|00:00:00"
        ]
    );
}
