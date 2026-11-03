//! Parse-frame references locate backend plans without persisting AST pointers.

use core::cell::Cell;

use crate::mem::arena::{Arena, ArenaList};
use crate::sql_err;

use super::eval::{SqlError, sqlstate};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Expression,
    Query,
    Statement,
}

#[derive(Clone, Copy)]
pub(super) struct Site<'a> {
    pub kind: Kind,
    pub key: usize,
    pub position: usize,
    pub text: &'a str,
}

struct Capture<'a> {
    body: &'a str,
    arena: &'a Arena,
    sites: ArenaList<'a, Site<'a>>,
}

thread_local! {
    static CAPTURE: Cell<*mut ()> = const { Cell::new(core::ptr::null_mut()) };
}

pub(super) fn capture<'a, T>(
    body: &'a str,
    arena: &'a Arena,
    parse: impl FnOnce() -> Result<T, SqlError>,
) -> Result<(T, &'a [Site<'a>]), SqlError> {
    struct Restore(*mut ());
    impl Drop for Restore {
        fn drop(&mut self) {
            CAPTURE.with(|slot| slot.set(self.0));
        }
    }
    let mut frame = Capture {
        body,
        arena,
        sites: ArenaList::new(arena),
    };
    let previous = CAPTURE.with(|slot| slot.replace((&mut frame as *mut Capture<'_>).cast()));
    let restore = Restore(previous);
    let value = parse();
    drop(restore);
    Ok((value?, frame.sites.as_slice()))
}

pub(super) fn record(kind: Kind, key: usize, text: &str, original: &str) -> Result<(), SqlError> {
    CAPTURE.with(|slot| {
        let pointer = slot.get();
        if pointer.is_null() {
            return Ok(());
        }
        // The closure-only capture boundary owns this frame until parsing ends.
        // Neither the frame pointer nor an arbitrary caller borrow can escape.
        let frame = unsafe { &mut *pointer.cast::<Capture<'_>>() };
        let start = (original.as_ptr() as usize)
            .checked_sub(frame.body.as_ptr() as usize)
            .ok_or_else(invalid_source)?;
        let end = start
            .checked_add(original.len())
            .ok_or_else(invalid_source)?;
        let original = frame.body.get(start..end).ok_or_else(invalid_source)?;
        let text = if text == original {
            original
        } else {
            frame
                .arena
                .alloc_str(text)
                .map_err(|_| super::query::arena_full_pub())?
        };
        frame
            .sites
            .push(Site {
                kind,
                key,
                position: start,
                text,
            })
            .map_err(|_| super::query::arena_full_pub())
    })
}

fn invalid_source() -> SqlError {
    sql_err!(
        sqlstate::INTERNAL_ERROR,
        "PL/pgSQL input plan has no original body span"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::Budget;

    #[test]
    fn parse_frames_copy_synthetic_sources_and_restore_nested_authority() {
        let mut budget = Budget::new(16 << 10);
        let arena = Arena::new(&mut budget, "PL source frames", 16 << 10).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let body = arena.alloc_str("RETURN 1; RETURN 2").unwrap();
            let nested_body = arena.alloc_str("RETURN 3").unwrap();
            let mut synthetic = *b"SELECT 1";
            let (_, outer) = capture(body, &arena, || {
                record(Kind::Expression, 1, &body[7..8], &body[7..8])?;
                let (_, nested) = capture(nested_body, &arena, || {
                    record(
                        Kind::Query,
                        2,
                        core::str::from_utf8(&synthetic).unwrap(),
                        nested_body,
                    )
                })?;
                assert_eq!(nested.len(), 1);
                assert_eq!(nested[0].text, "SELECT 1");
                synthetic.fill(b'x');
                assert_eq!(nested[0].text, "SELECT 1");
                let foreign = *b"RETURN 1";
                assert!(
                    record(
                        Kind::Statement,
                        3,
                        "SELECT 1",
                        core::str::from_utf8(&foreign).unwrap()
                    )
                    .is_err()
                );
                let failed: Result<((), &[Site<'_>]), SqlError> =
                    capture(nested_body, &arena, || Err(invalid_source()));
                assert!(failed.is_err());
                record(Kind::Expression, 4, &body[17..18], &body[17..18])
            })
            .unwrap();
            assert_eq!(outer.len(), 2);
            assert_eq!((outer[0].position, outer[0].text), (7, "1"));
            assert_eq!((outer[1].position, outer[1].text), (17, "2"));
            assert_eq!((outer[0].key, outer[1].key), (1, 4));
            record(Kind::Statement, 9, "not retained", "outside a parse frame").unwrap();
        });
    }
}
