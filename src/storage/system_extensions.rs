//! Extension packages built into pos3ql, found in PostgreSQL's `$system`
//! control directory. Their control files and scripts are PostgreSQL 18.6's
//! own `contrib/postgres_fdw` files, copied unchanged from the PostgreSQL
//! 18.6 Homebrew build (`share/postgresql/extension`, REL_18_6) and covered
//! by the PostgreSQL License. A package's `LANGUAGE C` routines bind to the
//! native routines pos3ql implements for its module.

/// A native module pos3ql implements in place of a shared library.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeModule {
    PostgresFdw,
}

impl NativeModule {
    /// The `module_pathname` a package's scripts substitute for
    /// `MODULE_PATHNAME`, and the `probin` of its routines.
    pub(crate) const fn path(self) -> &'static str {
        match self {
            Self::PostgresFdw => "$libdir/postgres_fdw",
        }
    }

    pub(crate) fn from_path(path: &str) -> Option<Self> {
        (path == Self::PostgresFdw.path()).then_some(Self::PostgresFdw)
    }
}

/// One built-in package: its name, control file and version scripts as
/// `(from, to, source)`.
pub(crate) struct SystemPackage {
    pub name: &'static str,
    pub module: NativeModule,
    pub control: &'static str,
    pub scripts: &'static [(Option<&'static str>, &'static str, &'static str)],
}

pub(crate) const SYSTEM_PACKAGES: &[SystemPackage] = &[SystemPackage {
    name: "postgres_fdw",
    module: NativeModule::PostgresFdw,
    control: include_str!("system_extensions/postgres_fdw/postgres_fdw.control"),
    scripts: &[
        (
            None,
            "1.0",
            include_str!("system_extensions/postgres_fdw/postgres_fdw--1.0.sql"),
        ),
        (
            Some("1.0"),
            "1.1",
            include_str!("system_extensions/postgres_fdw/postgres_fdw--1.0--1.1.sql"),
        ),
        (
            Some("1.1"),
            "1.2",
            include_str!("system_extensions/postgres_fdw/postgres_fdw--1.1--1.2.sql"),
        ),
    ],
}];

/// A routine of a [`NativeModule`] that a `LANGUAGE C` routine binds to by
/// its link symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeRoutine {
    PostgresFdwHandler,
    PostgresFdwValidator,
    PostgresFdwGetConnections,
    PostgresFdwGetConnections12,
    PostgresFdwDisconnect,
    PostgresFdwDisconnectAll,
}

impl NativeRoutine {
    const ALL: [Self; 6] = [
        Self::PostgresFdwHandler,
        Self::PostgresFdwValidator,
        Self::PostgresFdwGetConnections,
        Self::PostgresFdwGetConnections12,
        Self::PostgresFdwDisconnect,
        Self::PostgresFdwDisconnectAll,
    ];

    pub(crate) const fn module(self) -> NativeModule {
        NativeModule::PostgresFdw
    }

    /// The link symbol, `prosrc` in `pg_proc`.
    pub(crate) const fn symbol(self) -> &'static str {
        match self {
            Self::PostgresFdwHandler => "postgres_fdw_handler",
            Self::PostgresFdwValidator => "postgres_fdw_validator",
            Self::PostgresFdwGetConnections => "postgres_fdw_get_connections",
            Self::PostgresFdwGetConnections12 => "postgres_fdw_get_connections_1_2",
            Self::PostgresFdwDisconnect => "postgres_fdw_disconnect",
            Self::PostgresFdwDisconnectAll => "postgres_fdw_disconnect_all",
        }
    }

    pub(crate) fn from_symbol(module: NativeModule, symbol: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|routine| routine.module() == module && routine.symbol() == symbol)
    }

    /// The durable code; see `RoutineLanguage::code`.
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::PostgresFdwHandler => 0,
            Self::PostgresFdwValidator => 1,
            Self::PostgresFdwGetConnections => 2,
            Self::PostgresFdwGetConnections12 => 3,
            Self::PostgresFdwDisconnect => 4,
            Self::PostgresFdwDisconnectAll => 5,
        }
    }

    pub(crate) fn from_code(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|routine| routine.code() == code)
    }
}
