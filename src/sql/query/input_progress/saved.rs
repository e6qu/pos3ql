//! Suspended inputs retain checked images; attempt-local views borrow scratch.

use super::*;
use crate::mem::arena::{ArenaHandle, ArenaSliceHandle};

fn retain_bytes(bytes: &[u8], arena: &Arena) -> Result<ArenaSliceHandle<u8>, SqlError> {
    arena
        .retain_slice_with(bytes.len(), |index| bytes[index])
        .map_err(|_| full())
}

fn restore_slice<'out, T: Copy + 'static>(
    source: &Arena,
    handle: ArenaSliceHandle<T>,
    output: &'out Arena,
) -> Result<&'out [T], SqlError> {
    let values = source.retained_slice_mut(handle).map_err(|_| changed())?;
    output
        .alloc_slice_copy(&values)
        .map(|values| &*values)
        .map_err(|_| full())
}

#[derive(Clone, Copy)]
struct OwnedDefault {
    mode: u8,
    sql: Option<ArenaSliceHandle<u8>>,
}

#[derive(Clone, Copy)]
struct OwnedInputDefinition {
    signature: OwnedRoutineCallContract,
    defaults: ArenaSliceHandle<OwnedDefault>,
    strict: bool,
    dependencies: ArenaSliceHandle<StoredQueryDependency>,
}

impl OwnedInputDefinition {
    fn capture(value: InputDefinition<'_>, arena: &Arena) -> Result<Self, SqlError> {
        let selected = value.signature;
        let copy = |values: &[crate::storage::RoutineArgumentDef]| {
            arena
                .retain_slice_with(values.len(), |index| values[index])
                .map_err(|_| full())
        };
        let signature = OwnedRoutineCallContract {
            oid: selected.oid,
            arguments: copy(selected.arguments)?,
            parameters: copy(selected.parameters)?,
            result: selected.result,
            result_columns: copy(selected.result_columns)?,
        };
        let defaults = arena
            .retain_slice_with(value.defaults.len(), |_| OwnedDefault {
                mode: 0,
                sql: None,
            })
            .map_err(|_| full())?;
        {
            let mut copied = arena.retained_slice_mut(defaults).map_err(|_| changed())?;
            for (output, input) in copied.iter_mut().zip(value.defaults) {
                output.mode = input.mode;
                output.sql = input
                    .sql
                    .map(|sql| retain_bytes(sql.as_bytes(), arena))
                    .transpose()?;
            }
        }
        Ok(Self {
            signature,
            defaults,
            strict: value.strict,
            dependencies: arena
                .retain_slice_with(value.dependencies.len(), |index| value.dependencies[index])
                .map_err(|_| full())?,
        })
    }

    fn load<'out>(
        self,
        source: &Arena,
        output: &'out Arena,
    ) -> Result<InputDefinition<'out>, SqlError> {
        let signature = RoutineCallContract {
            oid: self.signature.oid,
            arguments: restore_slice(source, self.signature.arguments, output)?,
            parameters: restore_slice(source, self.signature.parameters, output)?,
            result: self.signature.result,
            result_columns: restore_slice(source, self.signature.result_columns, output)?,
        };
        let selected = source
            .retained_slice_mut(self.defaults)
            .map_err(|_| changed())?;
        let defaults = output
            .alloc_slice_with(selected.len(), |_| ParameterDefault { mode: 0, sql: None })
            .map_err(|_| full())?;
        for (restored, saved) in defaults.iter_mut().zip(selected.iter()) {
            restored.mode = saved.mode;
            restored.sql = saved
                .sql
                .map(|handle| {
                    core::str::from_utf8(restore_slice(source, handle, output)?)
                        .map_err(|_| changed())
                })
                .transpose()?;
        }
        Ok(InputDefinition {
            signature,
            defaults,
            strict: self.strict,
            dependencies: restore_slice(source, self.dependencies, output)?,
        })
    }
}

#[derive(Clone, Copy)]
enum OwnedInputState {
    Pending,
    Value {
        bytes: ArenaSliceHandle<u8>,
        next_input: usize,
        next_invocation: usize,
        next_call: usize,
        replay: Option<ArenaHandle<super::super::super::sequence::StatementReplayCursor>>,
    },
    Definition(ArenaHandle<OwnedInputDefinition>),
    CastDefinition(Option<ArenaHandle<super::super::super::eval::BoundCast>>),
}

#[derive(Clone, Copy)]
pub(super) struct OwnedInputFrame {
    site: OwnedInputSite,
    state: OwnedInputState,
}

impl OwnedInputFrame {
    pub(super) fn capture(frame: InputFrame<'_>, arena: &Arena) -> Result<Self, SqlError> {
        let site = frame
            .site
            .map_name(|name| retain_bytes(name.as_bytes(), arena))?;
        let state = match frame.state {
            InputState::Pending => OwnedInputState::Pending,
            InputState::Value {
                value,
                next_input,
                next_invocation,
                next_call,
                replay,
            } => {
                let bytes = match value {
                    InputValue::Evaluated(value) => {
                        let length = super::super::super::exec::projected_row_len(&[*value])?;
                        let handle = arena
                            .retain_slice_with(length, |_| 0u8)
                            .map_err(|_| full())?;
                        let mut bytes = arena.retained_slice_mut(handle).map_err(|_| changed())?;
                        super::super::super::exec::encode_projected_into(&[*value], &mut bytes)?;
                        handle
                    }
                    InputValue::Retained(handle) => {
                        drop(arena.retained_slice_mut(handle).map_err(|_| changed())?);
                        handle
                    }
                };
                OwnedInputState::Value {
                    bytes,
                    next_input,
                    next_invocation,
                    next_call,
                    replay: replay
                        .map(|cursor| arena.retain(*cursor).map_err(|_| full()))
                        .transpose()?,
                }
            }
            InputState::Definition(value) => {
                let definition = OwnedInputDefinition::capture(*value, arena)?;
                OwnedInputState::Definition(arena.retain(definition).map_err(|_| full())?)
            }
            InputState::CastDefinition(value) => OwnedInputState::CastDefinition(
                value
                    .map(|cast| arena.retain(*cast).map_err(|_| full()))
                    .transpose()?,
            ),
        };
        Ok(Self { site, state })
    }

    pub(super) fn load<'out>(
        self,
        source: &Arena,
        output: &'out Arena,
    ) -> Result<InputFrame<'out>, SqlError> {
        let site = self.site.map_name(|handle| {
            core::str::from_utf8(restore_slice(source, handle, output)?).map_err(|_| changed())
        })?;
        let state = match self.state {
            OwnedInputState::Pending => InputState::Pending,
            OwnedInputState::Value {
                bytes,
                next_input,
                next_invocation,
                next_call,
                replay,
            } => InputState::Value {
                value: InputValue::Retained(bytes),
                next_input,
                next_invocation,
                next_call,
                replay: replay
                    .map(|handle| {
                        output
                            .alloc(source.retained(handle).ok_or_else(changed)?)
                            .map(|value| &*value)
                            .map_err(|_| full())
                    })
                    .transpose()?,
            },
            OwnedInputState::Definition(handle) => {
                let definition = source
                    .retained(handle)
                    .ok_or_else(changed)?
                    .load(source, output)?;
                InputState::Definition(output.alloc(definition).map_err(|_| full())?)
            }
            OwnedInputState::CastDefinition(value) => InputState::CastDefinition(
                value
                    .map(|handle| {
                        output
                            .alloc(source.retained(handle).ok_or_else(changed)?)
                            .map(|value| &*value)
                            .map_err(|_| full())
                    })
                    .transpose()?,
            ),
        };
        Ok(InputFrame { site, state })
    }

    pub(super) fn definition_oid(self, source: &Arena) -> Result<Option<i32>, SqlError> {
        match self.state {
            OwnedInputState::Definition(handle) => Ok(Some(
                source.retained(handle).ok_or_else(changed)?.signature.oid,
            )),
            _ => Ok(None),
        }
    }
}
